use std::sync::Arc;

use reed_solomon_erasure::galois_8::ReedSolomon;

use super::Backend;
use super::StorageError;
use super::bitrot::{md5_hex, sha256_hex};
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

pub fn encode_and_write(
    disks: &[Box<dyn Backend>],
    planner: &PlacementPlanner,
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    data: &[u8],
    opts: PutOptions,
) -> Result<ObjectInfo, StorageError> {
    encode_and_write_with_mrf(disks, planner, data_n, parity_n, bucket, key, data, opts, None)
}

#[allow(clippy::too_many_arguments)]
pub fn encode_and_write_with_mrf(
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
    encode_and_write_impl(disks, planner, data_n, parity_n, bucket, key, data, opts, mrf, None)
}

#[allow(clippy::too_many_arguments)]
pub fn encode_and_write_versioned(
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
    )
}

#[allow(clippy::too_many_arguments)]
fn encode_and_write_impl(
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
    let total = data_n + parity_n;
    let etag = md5_hex(data);
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
    let node_ids = placement
        .shards
        .iter()
        .map(|shard| shard.node_id.clone())
        .collect::<Vec<_>>();
    let volume_ids = placement
        .shards
        .iter()
        .map(|shard| shard.volume_id.clone())
        .collect::<Vec<_>>();

    // split data into data_n shards
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

    // write shards to disks according to distribution
    let mut errs: Vec<Option<StorageError>> = (0..total).map(|_| None).collect();

    // distribution[shard_idx] = disk_idx
    for (shard_idx, shard_data) in shards.iter().enumerate() {
        let disk_idx = distribution[shard_idx];
        let checksum = sha256_hex(shard_data);
        let meta = ObjectMeta {
            size: data.len() as u64,
            etag: etag.clone(),
            content_type: content_type.clone(),
            created_at,
            erasure: ErasureMeta {
                ftt: parity_n,
                index: shard_idx,
                distribution: distribution.clone(),
                epoch_id: placement.epoch_id,
                set_id: placement.set_id.clone(),
                node_ids: node_ids.clone(),
                volume_ids: volume_ids.clone(),
            },
            checksum,
            user_metadata: opts.user_metadata.clone(),
            tags: opts.tags.clone(),
            version_id: version_id.unwrap_or("").to_string(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
        };
        let write_result = if let Some(vid) = version_id {
            disks[disk_idx].write_versioned_shard(bucket, key, vid, shard_data, &meta)
        } else {
            disks[disk_idx].write_shard(bucket, key, shard_data, &meta)
        };
        if let Err(e) = write_result {
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
                let _ = disks[disk_idx].delete_object(bucket, key);
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
