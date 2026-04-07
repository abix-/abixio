use std::sync::Arc;

use reed_solomon_erasure::galois_8::ReedSolomon;

use super::Backend;
use super::StorageError;
use super::bitrot::{blake3_hex, md5_hex};
use super::metadata::{ErasureMeta, ObjectInfo, ObjectMeta, PutOptions, unix_timestamp_secs};
use crate::cluster::placement::PlacementPlanner;
use crate::heal::mrf::{MrfEntry, MrfQueue};

/// Compute a hash-based permutation of disk indices for a given key.
/// Same key always produces the same permutation.
pub fn hash_order(key: &str, n: usize) -> Vec<usize> {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(key.as_bytes());
    let mut indices: Vec<usize> = (0..n).collect();
    // Fisher-Yates shuffle seeded by hash bytes
    for i in (1..n).rev() {
        let h_idx = (i * 2) % hash.len();
        let rand_val = (hash[h_idx] as usize) << 8 | (hash[(h_idx + 1) % hash.len()] as usize);
        let j = rand_val % (i + 1);
        indices.swap(i, j);
    }
    indices
}

/// Split data into `count` equal-sized shards, padding the last shard with zeros.
pub fn split_data(data: &[u8], count: usize) -> Vec<Vec<u8>> {
    let shard_size = data.len().div_ceil(count);
    let shard_size = shard_size.max(1); // minimum 1 byte per shard
    let mut shards = Vec::with_capacity(count);
    for i in 0..count {
        let start = i * shard_size;
        if start >= data.len() {
            shards.push(vec![0u8; shard_size]);
        } else {
            let end = (start + shard_size).min(data.len());
            let mut shard = data[start..end].to_vec();
            // pad to shard_size
            shard.resize(shard_size, 0);
            shards.push(shard);
        }
    }
    shards
}

pub async fn encode_and_write(
    disks: &[Box<dyn Backend>],
    planner: &PlacementPlanner,
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    data: &[u8],
    opts: PutOptions,
) -> Result<ObjectInfo, StorageError> {
    encode_and_write_with_mrf(disks, planner, data_n, parity_n, bucket, key, data, opts, None).await
}

#[allow(clippy::too_many_arguments)]
pub async fn encode_and_write_with_mrf(
    disks: &[Box<dyn Backend>],
    planner: &PlacementPlanner,
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    data: &[u8],
    opts: PutOptions,
    mrf: Option<&Arc<MrfQueue>>,
) -> Result<ObjectInfo, StorageError> {
    encode_and_write_impl(disks, planner, data_n, parity_n, bucket, key, data, opts, mrf, None).await
}

#[allow(clippy::too_many_arguments)]
pub async fn encode_and_write_versioned(
    disks: &[Box<dyn Backend>],
    planner: &PlacementPlanner,
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    data: &[u8],
    opts: PutOptions,
    mrf: Option<&Arc<MrfQueue>>,
    version_id: &str,
) -> Result<ObjectInfo, StorageError> {
    encode_and_write_impl(
        disks,
        planner,
        data_n,
        parity_n,
        bucket,
        key,
        data,
        opts,
        mrf,
        Some(version_id),
    ).await
}

#[allow(clippy::too_many_arguments)]
async fn encode_and_write_impl(
    disks: &[Box<dyn Backend>],
    planner: &PlacementPlanner,
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    data: &[u8],
    opts: PutOptions,
    mrf: Option<&Arc<MrfQueue>>,
    version_id: Option<&str>,
) -> Result<ObjectInfo, StorageError> {
    let t0 = std::time::Instant::now();
    let total = data_n + parity_n;
    let etag = opts.precomputed_etag.clone().unwrap_or_else(|| md5_hex(data));
    let t_md5 = t0.elapsed();

    let created_at = unix_timestamp_secs();
    let content_type = if opts.content_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        opts.content_type.clone()
    };

    let placement = planner
        .plan(bucket, key, data_n, parity_n)
        .map_err(StorageError::InvalidConfig)?;
    let distribution = placement
        .shards
        .iter()
        .map(|shard| shard.backend_index)
        .collect::<Vec<_>>();
    let volume_ids = placement
        .shards
        .iter()
        .map(|shard| shard.volume_id.clone())
        .collect::<Vec<_>>();

    // split data into data_n shards
    let t1 = std::time::Instant::now();
    let mut shards = split_data(data, data_n);

    if parity_n > 0 {
        // add empty parity shards
        let shard_size = shards[0].len();
        for _ in 0..parity_n {
            shards.push(vec![0u8; shard_size]);
        }
        // encode parity
        let rs = ReedSolomon::new(data_n, parity_n)
            .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?;
        rs.encode(&mut shards)
            .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon encode: {}", e)))?;
    }
    let t_ec = t1.elapsed();

    // write shards to disks in parallel
    let t2 = std::time::Instant::now();
    let data_size = data.len() as u64;
    let vid_str = version_id.unwrap_or("").to_string();

    let t_sha_start = std::time::Instant::now();
    let checksums: Vec<String> = shards.iter().map(|s| blake3_hex(s)).collect();
    let t_sha = t_sha_start.elapsed();
    if data.len() >= 1024 * 1024 {
        tracing::info!(blake3_ms = t_sha.as_secs_f64() * 1000.0, shards = total, "shard checksums");
    }

    let write_futs: Vec<_> = shards
        .iter()
        .enumerate()
        .map(|(shard_idx, shard_data)| {
            let disk_idx = distribution[shard_idx];
            let checksum = checksums[shard_idx].clone();
            let meta = ObjectMeta {
                size: data_size,
                etag: etag.clone(),
                content_type: content_type.clone(),
                created_at,
                erasure: ErasureMeta {
                    ftt: parity_n,
                    index: shard_idx,
                    epoch_id: placement.epoch_id,
                    volume_ids: volume_ids.clone(),
                },
                checksum,
                user_metadata: opts.user_metadata.clone(),
                tags: opts.tags.clone(),
                version_id: vid_str.clone(),
                is_latest: true,
                is_delete_marker: false,
                parts: Vec::new(),
            };
            let disk = &disks[disk_idx];
            let vid = version_id;
            async move {
                let result = if let Some(vid) = vid {
                    disk.write_versioned_shard(bucket, key, vid, shard_data, &meta).await
                } else {
                    disk.write_shard(bucket, key, shard_data, &meta).await
                };
                (shard_idx, result)
            }
        })
        .collect();

    let write_results = futures::future::join_all(write_futs).await;
    let t_write = t2.elapsed();

    let t_total = t0.elapsed();
    if data.len() >= 1024 * 1024 {
        tracing::info!(
            size = data.len(),
            disks = total,
            md5_ms = t_md5.as_secs_f64() * 1000.0,
            ec_ms = t_ec.as_secs_f64() * 1000.0,
            write_ms = t_write.as_secs_f64() * 1000.0,
            total_ms = t_total.as_secs_f64() * 1000.0,
            "encode_and_write timing"
        );
    }

    let mut errs: Vec<Option<StorageError>> = (0..total).map(|_| None).collect();
    for (shard_idx, result) in write_results {
        if let Err(e) = result {
            errs[shard_idx] = Some(e);
        }
    }

    // check write quorum
    let successes = errs.iter().filter(|e| e.is_none()).count();
    let write_quorum = if parity_n == 0 { data_n } else { data_n + 1 };

    if successes < write_quorum {
        // rollback: delete from successful disks
        for (shard_idx, err) in errs.iter().enumerate() {
            if err.is_none() {
                let disk_idx = distribution[shard_idx];
                let _ = disks[disk_idx].delete_object(bucket, key).await;
            }
        }
        return Err(StorageError::WriteQuorum);
    }

    // enqueue to MRF if quorum met but some shards failed
    if successes < total
        && let Some(mrf) = mrf
    {
        let _ = mrf.enqueue(MrfEntry {
            bucket: bucket.to_string(),
            key: key.to_string(),
        });
    }

    Ok(ObjectInfo {
        bucket: bucket.to_string(),
        key: key.to_string(),
        size: data.len() as u64,
        etag,
        content_type,
        created_at,
        user_metadata: opts.user_metadata,
        tags: opts.tags,
        version_id: version_id.unwrap_or("").to_string(),
        is_delete_marker: false,
    })
}

/// Block size for streaming encode (1MB).
const STREAM_BLOCK_SIZE: usize = 1024 * 1024;

/// Streaming encode: reads body chunks, computes MD5 inline, encodes and writes
/// shard data block-by-block. Avoids buffering the entire object in memory before
/// processing (though individual blocks are still buffered).
#[allow(clippy::too_many_arguments)]
pub async fn encode_and_write_stream<S>(
    disks: &[Box<dyn Backend>],
    planner: &PlacementPlanner,
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    mut body: S,
    opts: PutOptions,
    mrf: Option<&Arc<MrfQueue>>,
) -> Result<ObjectInfo, StorageError>
where
    S: futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    use futures::StreamExt;
    use md5::{Digest, Md5};
    use tokio::io::AsyncWriteExt;

    let total = data_n + parity_n;
    let created_at = unix_timestamp_secs();
    let content_type = if opts.content_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        opts.content_type.clone()
    };

    let placement = planner
        .plan(bucket, key, data_n, parity_n)
        .map_err(StorageError::InvalidConfig)?;
    let distribution: Vec<usize> = placement
        .shards
        .iter()
        .map(|s| s.backend_index)
        .collect();
    let volume_ids: Vec<String> = placement
        .shards
        .iter()
        .map(|s| s.volume_id.clone())
        .collect();

    let rs = if parity_n > 0 {
        Some(
            ReedSolomon::new(data_n, parity_n)
                .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?,
        )
    } else {
        None
    };

    // open shard files: create dirs and open for writing
    let mut shard_files = Vec::with_capacity(total);
    let mut shard_paths = Vec::with_capacity(total);
    for shard_idx in 0..total {
        let disk_idx = distribution[shard_idx];
        let info = disks[disk_idx].info();
        let disk_root = if let Some(path) = info.label.strip_prefix("local:") {
            std::path::PathBuf::from(path)
        } else {
            return Err(StorageError::InvalidConfig(
                "streaming encode only supports local disks".to_string(),
            ));
        };
        let obj_dir = super::pathing::object_dir(&disk_root, bucket, key)?;
        tokio::fs::create_dir_all(&obj_dir).await?;
        let shard_path = super::pathing::object_shard_path(&disk_root, bucket, key)?;
        let file = tokio::fs::File::create(&shard_path).await?;
        shard_paths.push(shard_path);
        shard_files.push(file);
    }

    // running hashers
    let mut md5_hasher = Md5::new();
    let mut shard_hashers: Vec<blake3::Hasher> =
        (0..total).map(|_| blake3::Hasher::new()).collect();
    let mut total_size: u64 = 0;

    // read body in blocks, encode, write
    let mut block_buf = Vec::with_capacity(STREAM_BLOCK_SIZE * 2);

    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| StorageError::Io(e))?;
        md5_hasher.update(&chunk);
        total_size += chunk.len() as u64;
        block_buf.extend_from_slice(&chunk);

        // process complete blocks
        while block_buf.len() >= STREAM_BLOCK_SIZE {
            write_stream_block(
                &block_buf[..STREAM_BLOCK_SIZE],
                data_n,
                parity_n,
                &rs,
                &mut shard_files,
                &mut shard_hashers,
            )
            .await?;
            // shift remaining data to front (avoids alloc)
            let remaining = block_buf.len() - STREAM_BLOCK_SIZE;
            block_buf.copy_within(STREAM_BLOCK_SIZE.., 0);
            block_buf.truncate(remaining);
        }
    }

    // process remaining data (last partial block)
    if !block_buf.is_empty() {
        write_stream_block(
            &block_buf,
            data_n,
            parity_n,
            &rs,
            &mut shard_files,
            &mut shard_hashers,
        )
        .await?;
    }

    // flush and close shard files
    for file in &mut shard_files {
        file.flush().await?;
    }
    drop(shard_files);

    // finalize
    let etag = hex::encode(md5_hasher.finalize());
    let checksums: Vec<String> = shard_hashers
        .iter()
        .map(|h| h.finalize().to_hex().to_string())
        .collect();

    // write meta.json on each disk in parallel
    let meta_futs: Vec<_> = (0..total)
        .map(|shard_idx| {
            let disk_idx = distribution[shard_idx];
            let meta = ObjectMeta {
                size: total_size,
                etag: etag.clone(),
                content_type: content_type.clone(),
                created_at,
                erasure: ErasureMeta {
                    ftt: parity_n,
                    index: shard_idx,
                    epoch_id: placement.epoch_id,
                    volume_ids: volume_ids.clone(),
                },
                checksum: checksums[shard_idx].clone(),
                user_metadata: opts.user_metadata.clone(),
                tags: opts.tags.clone(),
                version_id: String::new(),
                is_latest: true,
                is_delete_marker: false,
                parts: Vec::new(),
            };
            let mf = super::metadata::ObjectMetaFile {
                versions: vec![meta],
            };
            let info = disks[disk_idx].info();
            let disk_root = std::path::PathBuf::from(info.label.strip_prefix("local:").unwrap());
            async move {
                let meta_path = super::pathing::object_meta_path(&disk_root, bucket, key)?;
                super::metadata::write_meta_file(&meta_path, &mf)
                    .await
                    .map_err(StorageError::Io)?;
                Ok::<_, StorageError>(())
            }
        })
        .collect();
    let meta_results = futures::future::join_all(meta_futs).await;
    let meta_errs: Vec<Option<StorageError>> = meta_results
        .into_iter()
        .map(|r| r.err())
        .collect();

    // check write quorum
    let successes = meta_errs.iter().filter(|e| e.is_none()).count();
    let write_quorum = if parity_n == 0 { data_n } else { data_n + 1 };
    if successes < write_quorum {
        // cleanup
        for path in &shard_paths {
            let _ = tokio::fs::remove_file(path).await;
        }
        return Err(StorageError::WriteQuorum);
    }

    // enqueue MRF if some shards failed
    if successes < total {
        if let Some(mrf) = mrf {
            let _ = mrf.enqueue(MrfEntry {
                bucket: bucket.to_string(),
                key: key.to_string(),
            });
        }
    }

    Ok(ObjectInfo {
        bucket: bucket.to_string(),
        key: key.to_string(),
        size: total_size,
        etag,
        content_type,
        created_at,
        user_metadata: opts.user_metadata,
        tags: opts.tags,
        version_id: String::new(),
        is_delete_marker: false,
    })
}

/// Write one block of data through the RS encode pipeline to all shard files.
async fn write_stream_block(
    block: &[u8],
    data_n: usize,
    parity_n: usize,
    rs: &Option<ReedSolomon>,
    shard_files: &mut [tokio::fs::File],
    shard_hashers: &mut [blake3::Hasher],
) -> Result<(), StorageError> {
    use tokio::io::AsyncWriteExt;

    let mut shards = split_data(block, data_n);
    if let Some(rs) = rs {
        let shard_size = shards[0].len();
        for _ in 0..parity_n {
            shards.push(vec![0u8; shard_size]);
        }
        rs.encode(&mut shards)
            .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon encode: {}", e)))?;
    }

    // update running hashes (CPU, fast)
    for (i, shard_data) in shards.iter().enumerate() {
        shard_hashers[i].update(shard_data);
    }

    // write all shard chunks in parallel
    let write_futs: Vec<_> = shards
        .iter()
        .zip(shard_files.iter_mut())
        .map(|(data, file)| file.write_all(data))
        .collect();
    let results = futures::future::join_all(write_futs).await;
    for r in results {
        r?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_order_deterministic() {
        let a = hash_order("photos/cat.jpg", 4);
        let b = hash_order("photos/cat.jpg", 4);
        assert_eq!(a, b);
    }

    #[test]
    fn hash_order_is_permutation() {
        let order = hash_order("photos/cat.jpg", 4);
        assert_eq!(order.len(), 4);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2, 3]);
    }

    #[test]
    fn hash_order_different_keys_differ() {
        let a = hash_order("photos/cat.jpg", 6);
        let b = hash_order("docs/tax.pdf", 6);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_order_single_disk() {
        let order = hash_order("any/key", 1);
        assert_eq!(order, vec![0]);
    }

    #[test]
    fn split_data_even() {
        let shards = split_data(b"abcdef", 3);
        assert_eq!(shards.len(), 3);
        assert_eq!(shards[0], b"ab");
        assert_eq!(shards[1], b"cd");
        assert_eq!(shards[2], b"ef");
    }

    #[test]
    fn split_data_uneven_pads() {
        let shards = split_data(b"abcde", 3);
        assert_eq!(shards.len(), 3);
        // shard_size = ceil(5/3) = 2
        assert_eq!(shards[0], b"ab");
        assert_eq!(shards[1], b"cd");
        assert_eq!(shards[2], b"e\0");
    }

    #[test]
    fn split_data_single_shard() {
        let shards = split_data(b"hello", 1);
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0], b"hello");
    }

    #[test]
    fn split_data_empty() {
        let shards = split_data(b"", 2);
        assert_eq!(shards.len(), 2);
        assert_eq!(shards[0], vec![0u8; 1]); // min shard size 1
        assert_eq!(shards[1], vec![0u8; 1]);
    }
}
