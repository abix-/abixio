use reed_solomon_erasure::galois_8::ReedSolomon;
use tokio::io::AsyncReadExt;

use super::Backend;
use super::StorageError;
use super::bitrot::verify_shard_checksum;
use super::metadata::ObjectMeta;

/// Block size matching the encode path (erasure_encode::STREAM_BLOCK_SIZE).
const STREAM_BLOCK_SIZE: usize = 1024 * 1024;

pub async fn read_and_decode(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
    let total = data_n + parity_n;

    // read meta + shard from all disks in parallel
    let read_futs: Vec<_> = disks.iter().map(|disk| disk.read_shard(bucket, key)).collect();
    let raw_results = futures::future::join_all(read_futs).await;
    let mut raw_reads: Vec<Option<(Vec<u8>, ObjectMeta)>> = raw_results
        .into_iter()
        .map(|r| r.ok())
        .collect();

    // if no disk has the object at all, it's ObjectNotFound, not ReadQuorum
    let any_found = raw_reads.iter().any(|r| r.is_some());
    if !any_found {
        return Err(StorageError::ObjectNotFound);
    }

    // place shards in correct shard-index positions using each disk's erasure.index
    let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
    let mut good_meta: Option<ObjectMeta> = None;

    for read in raw_reads.iter_mut() {
        if let Some((data, meta)) = read.take() {
            // bitrot check
            if !verify_shard_checksum(&data, &meta.checksum) {
                continue; // treat as missing
            }
            let shard_idx = meta.erasure.index;
            if shard_idx < total {
                if good_meta.is_none() {
                    good_meta = Some(meta);
                }
                shard_slots[shard_idx] = Some(data);
            }
        }
    }

    let good_meta = good_meta.ok_or(StorageError::ReadQuorum)?;

    // check read quorum
    let good_count = shard_slots.iter().filter(|s| s.is_some()).count();
    if good_count < data_n {
        return Err(StorageError::ReadQuorum);
    }

    // reconstruct if needed (only when parity > 0)
    if parity_n > 0 && good_count < total {
        let rs = ReedSolomon::new(data_n, parity_n)
            .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?;
        rs.reconstruct(&mut shard_slots)
            .map_err(|_| StorageError::ReadQuorum)?;
    }

    // join data shards (first data_n shards)
    let original_size = good_meta.size as usize;
    let mut result = Vec::with_capacity(original_size);
    for data in shard_slots.iter().take(data_n).flatten() {
        result.extend_from_slice(data);
    }
    // trim to original size (split may have padded)
    result.truncate(original_size);

    Ok((result, good_meta))
}

/// Decode block size for streaming. Larger = fewer iterations and syscalls.
/// 4MB matches RustFS's duplex buffer size and gives good throughput.
const DECODE_BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// Streaming decode using mmap: returns metadata + a stream of decoded chunks.
/// Shard files are memory-mapped for zero-copy reads. Decoded data flows
/// through a tokio duplex pipe (4MB buffer) to avoid per-chunk allocation.
pub async fn read_and_decode_stream(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
) -> Result<(ObjectMeta, futures::channel::mpsc::Receiver<Result<bytes::Bytes, StorageError>>), StorageError> {
    let total = data_n + parity_n;

    // mmap shard files from all disks in parallel
    let mmap_futs: Vec<_> = disks
        .iter()
        .map(|disk| disk.mmap_shard(bucket, key))
        .collect();
    let raw_results = futures::future::join_all(mmap_futs).await;

    let any_found = raw_results.iter().any(|r| r.is_ok());
    if !any_found {
        return Err(StorageError::ObjectNotFound);
    }

    // place mmaps in shard-index positions using metadata
    let mut mmap_slots: Vec<Option<super::MmapOrVec>> = (0..total).map(|_| None).collect();
    let mut good_meta: Option<ObjectMeta> = None;

    for result in raw_results {
        if let Ok((mmap, meta)) = result {
            let shard_idx = meta.erasure.index;
            if shard_idx < total {
                if good_meta.is_none() {
                    good_meta = Some(meta);
                }
                mmap_slots[shard_idx] = Some(mmap);
            }
        }
    }

    let good_meta = good_meta.ok_or(StorageError::ReadQuorum)?;

    let good_count = mmap_slots.iter().filter(|s| s.is_some()).count();
    if good_count < data_n {
        return Err(StorageError::ReadQuorum);
    }

    let original_size = good_meta.size as usize;
    let meta_clone = good_meta.clone();

    let (tx, rx) = futures::channel::mpsc::channel(4);

    let dn = data_n;
    let pn = parity_n;
    tokio::spawn(async move {
        use futures::SinkExt;
        let mut tx = tx;

        let rs = if pn > 0 {
            ReedSolomon::new(dn, pn).ok()
        } else {
            None
        };

        let total = dn + pn;
        let mut remaining = original_size;
        let mut shard_read_offset = 0usize;

        while remaining > 0 {
            // process up to encode_blocks_per_iter encode-blocks in one iteration
            let iter_size = remaining.min(DECODE_BLOCK_SIZE);
            let mut iter_data = Vec::with_capacity(iter_size);
            let mut iter_remaining = iter_size;

            while iter_remaining > 0 {
                let block_size = iter_remaining.min(STREAM_BLOCK_SIZE);
                let shard_chunk_size = block_size.div_ceil(dn).max(1);

                // slice directly from mmap (zero-copy read)
                let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
                for (i, mmap_opt) in mmap_slots.iter().enumerate() {
                    if let Some(mmap) = mmap_opt {
                        let end = (shard_read_offset + shard_chunk_size).min(mmap.len());
                        if shard_read_offset < mmap.len() && end > shard_read_offset {
                            shard_slots[i] = Some(mmap[shard_read_offset..end].to_vec());
                        }
                    }
                }

                let good = shard_slots.iter().filter(|s| s.is_some()).count();
                if good < dn {
                    let _ = tx.send(Err(StorageError::ReadQuorum)).await;
                    return;
                }

                if pn > 0 && good < total {
                    if let Some(ref rs) = rs {
                        if rs.reconstruct(&mut shard_slots).is_err() {
                            let _ = tx.send(Err(StorageError::ReadQuorum)).await;
                            return;
                        }
                    }
                }

                let mut block_data = Vec::with_capacity(block_size);
                for shard in shard_slots.iter().take(dn).flatten() {
                    block_data.extend_from_slice(shard);
                }
                block_data.truncate(block_size);

                shard_read_offset += shard_chunk_size;
                iter_remaining -= block_size;
                iter_data.extend_from_slice(&block_data);
            }

            iter_data.truncate(iter_size);
            remaining -= iter_size;

            if tx.send(Ok(bytes::Bytes::from(iter_data))).await.is_err() {
                return;
            }
        }
    });

    Ok((meta_clone, rx))
}

/// Decode a multipart object by reading and decoding each part's shards
/// independently, then concatenating the results.
pub async fn read_and_decode_multipart(
    disks: &[Box<dyn Backend>],
    bucket: &str,
    key: &str,
    meta: &ObjectMeta,
) -> Result<Vec<u8>, StorageError> {
    let mut result = Vec::with_capacity(meta.size as usize);

    for part in &meta.parts {
        let data_n = part.erasure.data();
        let parity_n = part.erasure.parity();
        let total = data_n + parity_n;
        let part_file = format!("part.{}", part.number);

        // read this part's shard from each disk, use erasure.index to slot
        let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
        for disk in disks.iter() {
            let info = disk.info();
            let root = if let Some(path) = info.label.strip_prefix("local:") {
                std::path::PathBuf::from(path)
            } else {
                continue;
            };
            let obj_dir = match super::pathing::object_dir(&root, bucket, key) {
                Ok(d) => d,
                Err(_) => continue,
            };
            // read part meta to get shard index for this disk
            let meta_file = format!("part.{}.meta", part.number);
            let meta_path = obj_dir.join(&meta_file);
            let shard_idx = if let Ok(meta_data) = tokio::fs::read(&meta_path).await {
                if let Ok(pm) = serde_json::from_slice::<crate::multipart::PartFileMeta>(&meta_data) {
                    pm.erasure.index
                } else {
                    continue;
                }
            } else {
                continue;
            };
            let part_path = obj_dir.join(&part_file);
            if let Ok(data) = tokio::fs::read(&part_path).await {
                if shard_idx < total {
                    shard_slots[shard_idx] = Some(data);
                }
            }
        }

        let good_count = shard_slots.iter().filter(|s| s.is_some()).count();
        if good_count < data_n {
            return Err(StorageError::ReadQuorum);
        }

        if parity_n > 0 && good_count < total {
            let rs = ReedSolomon::new(data_n, parity_n)
                .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?;
            rs.reconstruct(&mut shard_slots)
                .map_err(|_| StorageError::ReadQuorum)?;
        }

        let mut part_data = Vec::with_capacity(part.size as usize);
        for data in shard_slots.iter().take(data_n).flatten() {
            part_data.extend_from_slice(data);
        }
        part_data.truncate(part.size as usize);
        result.extend_from_slice(&part_data);
    }

    Ok(result)
}

pub async fn read_and_decode_versioned(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    version_id: &str,
) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
    let total = data_n + parity_n;

    let read_futs: Vec<_> = disks
        .iter()
        .map(|disk| disk.read_versioned_shard(bucket, key, version_id))
        .collect();
    let raw_results = futures::future::join_all(read_futs).await;
    let mut raw_reads: Vec<Option<(Vec<u8>, ObjectMeta)>> = raw_results
        .into_iter()
        .map(|r| r.ok())
        .collect();

    let any_found = raw_reads.iter().any(|r| r.is_some());
    if !any_found {
        return Err(StorageError::ObjectNotFound);
    }

    let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
    let mut good_meta: Option<ObjectMeta> = None;

    for read in raw_reads.iter_mut() {
        if let Some((data, meta)) = read.take() {
            if !verify_shard_checksum(&data, &meta.checksum) {
                continue;
            }
            let shard_idx = meta.erasure.index;
            if shard_idx < total {
                if good_meta.is_none() {
                    good_meta = Some(meta);
                }
                shard_slots[shard_idx] = Some(data);
            }
        }
    }

    let good_meta = good_meta.ok_or(StorageError::ReadQuorum)?;

    let good_count = shard_slots.iter().filter(|s| s.is_some()).count();
    if good_count < data_n {
        return Err(StorageError::ReadQuorum);
    }

    if parity_n > 0 && good_count < total {
        let rs = ReedSolomon::new(data_n, parity_n)
            .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?;
        rs.reconstruct(&mut shard_slots)
            .map_err(|_| StorageError::ReadQuorum)?;
    }

    let original_size = good_meta.size as usize;
    let mut result = Vec::with_capacity(original_size);
    for data in shard_slots.iter().take(data_n).flatten() {
        result.extend_from_slice(data);
    }
    result.truncate(original_size);

    Ok((result, good_meta))
}
