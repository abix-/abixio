use reed_solomon_erasure::galois_8::ReedSolomon;

use super::Backend;
use super::StorageError;
use super::bitrot::sha256_hex;
use super::metadata::ObjectMeta;

pub fn read_and_decode(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
    let total = data_n + parity_n;

    // read meta + shard from all disks
    let mut raw_reads: Vec<Option<(Vec<u8>, ObjectMeta)>> = Vec::with_capacity(total);
    for disk in disks.iter() {
        match disk.read_shard(bucket, key) {
            Ok(pair) => raw_reads.push(Some(pair)),
            Err(_) => raw_reads.push(None),
        }
    }

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
            if sha256_hex(&data) != meta.checksum {
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

/// Decode a multipart object by reading and decoding each part's shards
/// independently, then concatenating the results.
pub fn read_and_decode_multipart(
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
            let shard_idx = if let Ok(meta_data) = std::fs::read(&meta_path) {
                if let Ok(pm) = serde_json::from_slice::<crate::multipart::PartFileMeta>(&meta_data) {
                    pm.erasure.index
                } else {
                    continue;
                }
            } else {
                continue;
            };
            let part_path = obj_dir.join(&part_file);
            if let Ok(data) = std::fs::read(&part_path) {
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

pub fn read_and_decode_versioned(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    version_id: &str,
) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
    let total = data_n + parity_n;

    let mut raw_reads: Vec<Option<(Vec<u8>, ObjectMeta)>> = Vec::with_capacity(total);
    for disk in disks.iter() {
        match disk.read_versioned_shard(bucket, key, version_id) {
            Ok(pair) => raw_reads.push(Some(pair)),
            Err(_) => raw_reads.push(None),
        }
    }

    let any_found = raw_reads.iter().any(|r| r.is_some());
    if !any_found {
        return Err(StorageError::ObjectNotFound);
    }

    let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
    let mut good_meta: Option<ObjectMeta> = None;

    for read in raw_reads.iter_mut() {
        if let Some((data, meta)) = read.take() {
            if sha256_hex(&data) != meta.checksum {
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
