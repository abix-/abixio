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

    // extract distribution from first available meta
    // if no disk has the object at all, it's ObjectNotFound, not ReadQuorum
    let any_found = raw_reads.iter().any(|r| r.is_some());
    if !any_found {
        return Err(StorageError::ObjectNotFound);
    }
    let distribution = raw_reads
        .iter()
        .flatten()
        .next()
        .map(|(_, meta)| meta.erasure.distribution.clone())
        .ok_or(StorageError::ReadQuorum)?;

    // place shards in correct shard-index positions using distribution
    // distribution[shard_idx] = disk_idx
    let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
    let mut good_meta: Option<ObjectMeta> = None;

    for (shard_idx, &disk_idx) in distribution.iter().enumerate() {
        if disk_idx >= disks.len() {
            continue;
        }
        if let Some((data, meta)) = raw_reads[disk_idx].take() {
            // bitrot check
            if sha256_hex(&data) != meta.checksum {
                continue; // treat as missing
            }
            if good_meta.is_none() {
                good_meta = Some(meta);
            }
            shard_slots[shard_idx] = Some(data);
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
        let distribution = &part.erasure.distribution;
        let part_file = format!("part.{}", part.number);

        // read this part's shard from each disk using the part's distribution
        let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
        for (shard_idx, &disk_idx) in distribution.iter().enumerate() {
            if disk_idx >= disks.len() {
                continue;
            }
            // read part.N from this disk's object directory
            let info = disks[disk_idx].info();
            let root = if let Some(path) = info.label.strip_prefix("local:") {
                std::path::PathBuf::from(path)
            } else {
                continue; // skip non-local disks for now
            };
            let obj_dir = super::pathing::object_dir(&root, bucket, key)?;
            let part_path = obj_dir.join(&part_file);
            if let Ok(data) = std::fs::read(&part_path) {
                if sha256_hex(&data) == part.checksum || shard_idx != part.erasure.index {
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
    let distribution = raw_reads
        .iter()
        .flatten()
        .next()
        .map(|(_, meta)| meta.erasure.distribution.clone())
        .ok_or(StorageError::ReadQuorum)?;

    let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
    let mut good_meta: Option<ObjectMeta> = None;

    for (shard_idx, &disk_idx) in distribution.iter().enumerate() {
        if disk_idx >= disks.len() {
            continue;
        }
        if let Some((data, meta)) = raw_reads[disk_idx].take() {
            if sha256_hex(&data) != meta.checksum {
                continue;
            }
            if good_meta.is_none() {
                good_meta = Some(meta);
            }
            shard_slots[shard_idx] = Some(data);
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
