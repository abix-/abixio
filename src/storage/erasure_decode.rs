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

/// Streaming decode: returns metadata + a stream of decoded data chunks.
/// Instead of collecting the entire object into memory, reads shard files
/// in blocks and yields decoded data as it becomes available.
pub async fn read_and_decode_stream(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
) -> Result<(ObjectMeta, futures::channel::mpsc::Receiver<Result<bytes::Bytes, StorageError>>), StorageError> {
    let total = data_n + parity_n;

    // read metadata + open shard readers from all disks in parallel
    let read_futs: Vec<_> = disks
        .iter()
        .map(|disk| disk.read_shard_stream(bucket, key))
        .collect();
    let raw_results = futures::future::join_all(read_futs).await;

    let any_found = raw_results.iter().any(|r| r.is_ok());
    if !any_found {
        return Err(StorageError::ObjectNotFound);
    }

    // place readers in shard-index positions using metadata
    let mut reader_slots: Vec<Option<std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send + Unpin>>>> =
        (0..total).map(|_| None).collect();
    let mut good_meta: Option<ObjectMeta> = None;

    for result in raw_results {
        if let Ok((reader, meta)) = result {
            let shard_idx = meta.erasure.index;
            if shard_idx < total {
                if good_meta.is_none() {
                    good_meta = Some(meta);
                }
                reader_slots[shard_idx] = Some(reader);
            }
        }
    }

    let good_meta = good_meta.ok_or(StorageError::ReadQuorum)?;

    let good_count = reader_slots.iter().filter(|s| s.is_some()).count();
    if good_count < data_n {
        return Err(StorageError::ReadQuorum);
    }

    let original_size = good_meta.size as usize;
    let meta_clone = good_meta.clone();

    // channel for streaming decoded chunks back to caller
    let (tx, rx) = futures::channel::mpsc::channel(4);

    // spawn background task to read blocks, decode, and send
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

        while remaining > 0 {
            let block_size = remaining.min(STREAM_BLOCK_SIZE);
            let shard_chunk_size = block_size.div_ceil(dn).max(1);

            // read shard_chunk_size bytes from each available reader
            let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
            for (i, reader_opt) in reader_slots.iter_mut().enumerate() {
                if let Some(reader) = reader_opt {
                    let mut buf = vec![0u8; shard_chunk_size];
                    match reader.read_exact(&mut buf).await {
                        Ok(_) => { shard_slots[i] = Some(buf); }
                        Err(_) => { /* treat as missing shard */ }
                    }
                }
            }

            let good = shard_slots.iter().filter(|s| s.is_some()).count();
            if good < dn {
                let _ = tx.send(Err(StorageError::ReadQuorum)).await;
                return;
            }

            // RS reconstruct if needed
            if pn > 0 && good < total {
                if let Some(ref rs) = rs {
                    if rs.reconstruct(&mut shard_slots).is_err() {
                        let _ = tx.send(Err(StorageError::ReadQuorum)).await;
                        return;
                    }
                }
            }

            // join data shards
            let mut block_data = Vec::with_capacity(block_size);
            for shard in shard_slots.iter().take(dn).flatten() {
                block_data.extend_from_slice(shard);
            }
            block_data.truncate(block_size);

            remaining -= block_size;

            if tx.send(Ok(bytes::Bytes::from(block_data))).await.is_err() {
                return; // receiver dropped
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
