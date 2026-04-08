use std::sync::Arc;

use reed_solomon_erasure::galois_8::ReedSolomon;

use super::Backend;
use super::StorageError;
// blake3_hex and md5_hex kept for heal/decode paths
#[allow(unused_imports)]
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

/// Split data into pre-allocated shard buffers. Zero allocation -- reuses existing Vecs.
/// `buffers` must have at least `count` entries. Each buffer is cleared and filled.
fn split_data_into(data: &[u8], count: usize, buffers: &mut [Vec<u8>]) {
    let shard_size = data.len().div_ceil(count).max(1);
    for (i, buf) in buffers.iter_mut().enumerate().take(count) {
        buf.clear();
        let start = i * shard_size;
        if start >= data.len() {
            buf.resize(shard_size, 0);
        } else {
            let end = (start + shard_size).min(data.len());
            buf.extend_from_slice(&data[start..end]);
            buf.resize(shard_size, 0);
        }
    }
}

/// Block size for streaming encode (1MB).
const STREAM_BLOCK_SIZE: usize = 1024 * 1024;

use super::ShardWriter;

/// Single encode path. Streams body through RS encode, writes via ShardWriter trait.
/// Works for all backends (local, remote), versioned and unversioned.
#[allow(clippy::too_many_arguments)]
pub async fn encode_and_write<S>(
    disks: &[Box<dyn Backend>],
    planner: &PlacementPlanner,
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    mut body: S,
    opts: PutOptions,
    mrf: Option<&Arc<MrfQueue>>,
    version_id: Option<&str>,
) -> Result<ObjectInfo, StorageError>
where
    S: futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    use futures::StreamExt;
    use md5::{Digest, Md5};

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

    // open shard writers in parallel via Backend trait
    let open_futs: Vec<_> = (0..total)
        .map(|shard_idx| {
            let disk_idx = distribution[shard_idx];
            disks[disk_idx].open_shard_writer(bucket, key, version_id)
        })
        .collect();
    let mut writers: Vec<Box<dyn ShardWriter>> = futures::future::join_all(open_futs)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

    // running hashers: MD5 for etag (inline), blake3 per shard (inline)
    let mut md5_hasher = Md5::new();
    let mut shard_hashers: Vec<blake3::Hasher> =
        (0..total).map(|_| blake3::Hasher::new()).collect();
    let mut total_size: u64 = 0;

    // pre-allocate shard buffers (reused across all blocks -- zero alloc per block)
    let shard_capacity = STREAM_BLOCK_SIZE.div_ceil(data_n).max(1);
    let mut shard_bufs: Vec<Vec<u8>> = (0..total)
        .map(|_| Vec::with_capacity(shard_capacity))
        .collect();

    // read body in blocks, encode, write through ShardWriters
    let mut block_buf = Vec::with_capacity(STREAM_BLOCK_SIZE * 2);

    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(StorageError::Io)?;
        md5_hasher.update(&chunk);
        total_size += chunk.len() as u64;
        block_buf.extend_from_slice(&chunk);

        while block_buf.len() >= STREAM_BLOCK_SIZE {
            write_block(
                &block_buf[..STREAM_BLOCK_SIZE],
                data_n,
                parity_n,
                &rs,
                &mut writers,
                &mut shard_hashers,
                &mut shard_bufs,
            )
            .await?;
            let remaining = block_buf.len() - STREAM_BLOCK_SIZE;
            block_buf.copy_within(STREAM_BLOCK_SIZE.., 0);
            block_buf.truncate(remaining);
        }
    }

    // last partial block
    if !block_buf.is_empty() {
        write_block(
            &block_buf,
            data_n,
            parity_n,
            &rs,
            &mut writers,
            &mut shard_hashers,
            &mut shard_bufs,
        )
        .await?;
    }

    // finalize: compute hashes, build meta, call finalize on each writer
    let etag = opts
        .precomputed_etag
        .clone()
        .unwrap_or_else(|| hex::encode(md5_hasher.finalize()));
    let checksums: Vec<String> = shard_hashers
        .iter()
        .map(|h| h.finalize().to_hex().to_string())
        .collect();

    let vid_str = version_id.unwrap_or("").to_string();

    let mut errs: Vec<Option<StorageError>> = Vec::with_capacity(total);
    // finalize writers in parallel
    let finalize_futs: Vec<_> = writers
        .into_iter()
        .enumerate()
        .map(|(shard_idx, writer)| {
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
                version_id: vid_str.clone(),
                is_latest: true,
                is_delete_marker: false,
                parts: Vec::new(),
                inline_data: None,
            };
            async move { writer.finalize(&meta).await }
        })
        .collect();

    let finalize_results = futures::future::join_all(finalize_futs).await;
    for result in &finalize_results {
        errs.push(result.as_ref().err().map(|e| {
            StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
        }));
    }

    // check write quorum
    let successes = errs.iter().filter(|e| e.is_none()).count();
    let write_quorum = if parity_n == 0 { data_n } else { data_n + 1 };

    if successes < write_quorum {
        // rollback successful shards
        for (shard_idx, err) in errs.iter().enumerate() {
            if err.is_none() {
                let disk_idx = distribution[shard_idx];
                let _ = disks[disk_idx].delete_object(bucket, key).await;
            }
        }
        return Err(StorageError::WriteQuorum);
    }

    // enqueue to MRF if quorum met but some shards failed
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
        version_id: vid_str,
        is_delete_marker: false,
    })
}

/// Convenience: wrap &[u8] as a one-shot stream and call encode_and_write.
#[allow(clippy::too_many_arguments)]
pub async fn encode_and_write_bytes(
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
    let stream = futures::stream::once(async {
        Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::copy_from_slice(data))
    });
    futures::pin_mut!(stream);
    encode_and_write(disks, planner, data_n, parity_n, bucket, key, stream, opts, mrf, version_id).await
}

/// Write one RS-encoded block to all shard writers sequentially.
/// Uses pre-allocated shard buffers (zero allocation per block).
/// Parallel approaches (FuturesUnordered, channel-based tasks) were benchmarked
/// and regressed at 10MB due to task/channel overhead exceeding the benefit
/// when all shards write to the same physical disk.
async fn write_block(
    block: &[u8],
    data_n: usize,
    parity_n: usize,
    rs: &Option<ReedSolomon>,
    writers: &mut [Box<dyn ShardWriter>],
    shard_hashers: &mut [blake3::Hasher],
    shard_bufs: &mut Vec<Vec<u8>>,
) -> Result<(), StorageError> {
    let total = data_n + parity_n;

    // split data into pre-allocated buffers (zero alloc)
    split_data_into(block, data_n, shard_bufs);

    // zero parity buffers and RS encode
    if let Some(rs) = rs {
        let shard_size = shard_bufs[0].len();
        for buf in shard_bufs.iter_mut().skip(data_n).take(parity_n) {
            buf.clear();
            buf.resize(shard_size, 0);
        }
        rs.encode(&mut shard_bufs[..])
            .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon encode: {}", e)))?;
    }

    for i in 0..total {
        shard_hashers[i].update(&shard_bufs[i]);
        writers[i].write_chunk(&shard_bufs[i]).await?;
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
