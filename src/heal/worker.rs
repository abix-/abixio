use std::sync::Arc;
use std::time::Duration;

use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::storage::Backend;
use crate::storage::StorageError;
use crate::storage::bitrot::sha256_hex;
use crate::storage::metadata::ObjectMeta;
use crate::storage::pathing;

use super::mrf::{MrfEntry, MrfQueue};
use super::scanner::ScanState;

/// Result of a single object heal attempt.
#[derive(Debug)]
pub enum HealResult {
    /// Object was already healthy on all disks.
    Healthy,
    /// One or more shards were repaired.
    Repaired { shards_fixed: usize },
    /// Not enough healthy shards to reconstruct.
    Unrecoverable,
}

/// Per-disk shard status after inspection.
#[derive(Debug, Clone, PartialEq)]
enum ShardStatus {
    /// Shard + meta present and checksum valid.
    Good,
    /// Shard missing, corrupt, or meta mismatch -- needs repair.
    NeedsRepair,
}

/// Heal a single object by reading all disks, finding consensus metadata,
/// and reconstructing any missing or corrupt shards via Reed-Solomon.
/// EC params are derived from the object's stored metadata (per-object EC).
pub fn heal_object(
    disks: &[Box<dyn Backend>],
    bucket: &str,
    key: &str,
) -> Result<HealResult, StorageError> {
    pathing::validate_bucket_name(bucket)?;
    pathing::validate_object_key(key)?;
    // step 1: read meta + shard from every disk
    let mut reads: Vec<Option<(Vec<u8>, ObjectMeta)>> = Vec::with_capacity(disks.len());
    for disk in disks.iter() {
        match disk.read_shard(bucket, key) {
            Ok(pair) => reads.push(Some(pair)),
            Err(_) => reads.push(None),
        }
    }

    // derive EC params from stored metadata
    let first_meta = reads.iter().flatten().next().map(|(_, m)| m);
    let (data_n, parity_n) = match first_meta {
        Some(m) => (m.erasure.data(), m.erasure.parity()),
        None => return Err(StorageError::ObjectNotFound),
    };
    let total = data_n + parity_n;

    // step 2: find consensus metadata (majority by quorum_eq)
    let consensus_meta = find_consensus_meta(&reads, data_n)?;
    let distribution = &consensus_meta.erasure.distribution;

    // step 3: classify each shard position
    // distribution[shard_idx] = disk_idx
    let mut statuses = vec![ShardStatus::NeedsRepair; total];
    let mut shard_data: Vec<Option<Vec<u8>>> = vec![None; total];

    for (shard_idx, &disk_idx) in distribution.iter().enumerate() {
        if disk_idx >= disks.len() {
            continue;
        }
        if let Some((data, meta)) = &reads[disk_idx] {
            // check metadata consensus + checksum
            if meta.quorum_eq(&consensus_meta) && sha256_hex(data) == meta.checksum {
                statuses[shard_idx] = ShardStatus::Good;
                shard_data[shard_idx] = Some(data.clone());
            }
        }
    }

    let good_count = statuses.iter().filter(|s| **s == ShardStatus::Good).count();
    let needs_repair = statuses
        .iter()
        .filter(|s| **s == ShardStatus::NeedsRepair)
        .count();

    if needs_repair == 0 {
        return Ok(HealResult::Healthy);
    }

    if good_count < data_n {
        return Ok(HealResult::Unrecoverable);
    }

    // step 4: reconstruct missing shards
    if parity_n > 0 {
        let rs = ReedSolomon::new(data_n, parity_n)
            .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?;
        rs.reconstruct(&mut shard_data)
            .map_err(|_| StorageError::ReadQuorum)?;
    }

    // step 5: write repaired shards atomically
    let mut shards_fixed = 0;
    for (shard_idx, status) in statuses.iter().enumerate() {
        if *status != ShardStatus::NeedsRepair {
            continue;
        }
        let disk_idx = distribution[shard_idx];
        if disk_idx >= disks.len() {
            continue;
        }
        if let Some(data) = &shard_data[shard_idx] {
            let checksum = sha256_hex(data);
            let meta = ObjectMeta {
                erasure: crate::storage::metadata::ErasureMeta {
                    index: shard_idx,
                    ..consensus_meta.erasure.clone()
                },
                checksum,
                ..consensus_meta.clone()
            };
            if disks[disk_idx]
                .write_shard(bucket, key, data, &meta)
                .is_ok()
            {
                shards_fixed += 1;
            }
        }
    }

    Ok(HealResult::Repaired { shards_fixed })
}

/// Find the metadata that appears on a majority of disks (by quorum_eq).
fn find_consensus_meta(
    reads: &[Option<(Vec<u8>, ObjectMeta)>],
    data_n: usize,
) -> Result<ObjectMeta, StorageError> {
    // collect all valid metas
    let metas: Vec<&ObjectMeta> = reads.iter().flatten().map(|(_, m)| m).collect();
    if metas.is_empty() {
        return Err(StorageError::ObjectNotFound);
    }

    // count occurrences of each distinct meta (by quorum_eq)
    let mut groups: Vec<(&ObjectMeta, usize)> = Vec::new();
    for meta in &metas {
        if let Some(g) = groups.iter_mut().find(|(m, _)| m.quorum_eq(meta)) {
            g.1 += 1;
        } else {
            groups.push((meta, 1));
        }
    }

    // pick the one with highest count (must meet data_n quorum)
    groups.sort_by(|a, b| b.1.cmp(&a.1));
    let (best_meta, count) = groups[0];
    if count < data_n {
        return Err(StorageError::ReadQuorum);
    }

    Ok(best_meta.clone())
}

/// MRF drain worker: pulls entries from the queue and heals them.
pub async fn mrf_drain_worker(
    disks: Arc<Vec<Box<dyn Backend>>>,
    mrf: Arc<MrfQueue>,
    mut rx: tokio::sync::mpsc::Receiver<MrfEntry>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            Some(entry) = rx.recv() => {
                let result = tokio::task::spawn_blocking({
                    let disks = Arc::clone(&disks);
                    let bucket = entry.bucket.clone();
                    let key = entry.key.clone();
                    move || heal_object(&disks, &bucket, &key)
                })
                .await;

                match result {
                    Ok(Ok(HealResult::Repaired { shards_fixed })) => {
                        tracing::info!(
                            bucket = entry.bucket,
                            key = entry.key,
                            shards_fixed,
                            "healed object"
                        );
                    }
                    Ok(Ok(HealResult::Healthy)) => {}
                    Ok(Ok(HealResult::Unrecoverable)) => {
                        tracing::warn!(
                            bucket = entry.bucket,
                            key = entry.key,
                            "object unrecoverable, not enough healthy shards"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            bucket = entry.bucket,
                            key = entry.key,
                            error = %e,
                            "heal failed"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "heal task panicked");
                    }
                }

                mrf.mark_done(&entry);
            }
            _ = shutdown.changed() => {
                break;
            }
        }
    }
}

/// Background integrity scanner: walks all objects on a timer, checks
/// shard integrity, and enqueues degraded objects to the MRF queue.
pub async fn scanner_loop(
    disks: Arc<Vec<Box<dyn Backend>>>,
    mrf: Arc<MrfQueue>,
    scan_state: Arc<ScanState>,
    scan_interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        // run one scan cycle
        let scan_disks = Arc::clone(&disks);
        let scan_mrf = Arc::clone(&mrf);
        let scan_state2 = Arc::clone(&scan_state);

        let _ = tokio::task::spawn_blocking(move || {
            run_scan_cycle(&scan_disks, &scan_mrf, &scan_state2);
        })
        .await;

        // wait for next interval or shutdown
        tokio::select! {
            _ = tokio::time::sleep(scan_interval) => {}
            _ = shutdown.changed() => {
                break;
            }
        }
    }
}

/// Single scan cycle: enumerate all buckets/objects, check integrity.
fn run_scan_cycle(
    disks: &[Box<dyn Backend>],
    mrf: &MrfQueue,
    scan_state: &ScanState,
) {
    // list buckets from first responsive disk
    let buckets = match disks.first().and_then(|d| d.list_buckets().ok()) {
        Some(b) => b,
        None => return,
    };

    for bucket in &buckets {
        let keys = match disks.first().and_then(|d| d.list_objects(bucket, "").ok()) {
            Some(k) => k,
            None => continue,
        };

        for key in &keys {
            if !scan_state.should_scan(bucket, key) {
                continue;
            }

            // check shard health across all disks
            if object_needs_healing(disks, bucket, key) {
                let _ = mrf.enqueue(MrfEntry {
                    bucket: bucket.clone(),
                    key: key.clone(),
                });
            }

            scan_state.mark_checked(bucket, key);
        }
    }
}

/// Check if an object has any missing or corrupt shards.
/// EC params are read from the object's stored metadata.
fn object_needs_healing(
    disks: &[Box<dyn Backend>],
    bucket: &str,
    key: &str,
) -> bool {
    // read meta from first available disk to get distribution + EC params
    let mut first_meta: Option<ObjectMeta> = None;
    for disk in disks.iter() {
        if let Ok(meta) = disk.stat_object(bucket, key) {
            first_meta = Some(meta);
            break;
        }
    }
    let meta = match first_meta {
        Some(m) => m,
        None => return false, // object gone, nothing to heal
    };

    let total = meta.erasure.data() + meta.erasure.parity();
    let distribution = &meta.erasure.distribution;
    let mut good = 0;

    for (shard_idx, &disk_idx) in distribution.iter().enumerate() {
        if disk_idx >= disks.len() || shard_idx >= total {
            continue;
        }
        if let Ok((data, disk_meta)) = disks[disk_idx].read_shard(bucket, key)
            && disk_meta.quorum_eq(&meta)
            && sha256_hex(&data) == disk_meta.checksum
        {
            good += 1;
        }
    }

    good < total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Store;
    use crate::storage::volume_pool::VolumePool;
    use crate::storage::metadata::PutOptions;
    use tempfile::TempDir;

    fn make_disk_dirs(n: usize) -> (TempDir, Vec<std::path::PathBuf>) {
        let base = TempDir::new().unwrap();
        let mut paths = Vec::new();
        for i in 0..n {
            let p = base.path().join(format!("d{}", i));
            std::fs::create_dir_all(&p).unwrap();
            paths.push(p);
        }
        (base, paths)
    }

    fn make_backends(paths: &[std::path::PathBuf]) -> Vec<Box<dyn Backend>> {
        paths
            .iter()
            .map(|p| {
                Box::new(crate::storage::local_volume::LocalVolume::new(p.as_path()).unwrap())
                    as Box<dyn Backend>
            })
            .collect()
    }

    fn make_set(paths: &[std::path::PathBuf]) -> VolumePool {
        VolumePool::new(make_backends(paths)).unwrap()
    }

    fn put_test_object(set: &VolumePool, bucket: &str, key: &str, payload: &[u8]) {
        set.make_bucket(bucket).unwrap();
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object(bucket, key, payload, opts).unwrap();
    }

    #[test]
    fn heal_healthy_object_returns_healthy() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        put_test_object(&set, "test", "key", b"heal test data");

        let disks = make_backends(&paths);
        let result = heal_object(&disks, "test", "key").unwrap();
        assert!(matches!(result, HealResult::Healthy));
    }

    #[test]
    fn heal_missing_shard_repairs() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        put_test_object(&set, "test", "key", b"heal missing shard");

        // delete shard from one disk
        let obj_dir = paths[0].join("test").join("key");
        if obj_dir.exists() {
            std::fs::remove_dir_all(&obj_dir).unwrap();
        }

        let disks = make_backends(&paths);
        let result = heal_object(&disks, "test", "key").unwrap();
        assert!(matches!(result, HealResult::Repaired { shards_fixed } if shards_fixed >= 1));

        // verify the object is now fully readable
        let (data, _) = set.get_object("test", "key").unwrap();
        assert_eq!(data, b"heal missing shard");
    }

    #[test]
    fn heal_corrupt_shard_repairs() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        put_test_object(&set, "test", "key", b"heal corrupt shard");

        // corrupt shard.dat on disk 0
        let shard_path = paths[0].join("test").join("key").join("shard.dat");
        if shard_path.exists() {
            std::fs::write(&shard_path, b"CORRUPTED").unwrap();
        }

        let disks = make_backends(&paths);
        let result = heal_object(&disks, "test", "key").unwrap();
        assert!(matches!(result, HealResult::Repaired { shards_fixed } if shards_fixed >= 1));

        // verify data integrity after heal
        let (data, _) = set.get_object("test", "key").unwrap();
        assert_eq!(data, b"heal corrupt shard");
    }

    #[test]
    fn heal_too_many_missing_is_unrecoverable() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        put_test_object(&set, "test", "key", b"unrecoverable");

        // delete 3 of 4 disk shards (parity=2, so max tolerable is 2)
        for i in 0..3 {
            let obj_dir = paths[i].join("test").join("key");
            if obj_dir.exists() {
                std::fs::remove_dir_all(&obj_dir).unwrap();
            }
        }

        let disks = make_backends(&paths);
        let result = heal_object(&disks, "test", "key");
        // either Unrecoverable or ReadQuorum error -- both mean "can't heal"
        match result {
            Ok(HealResult::Unrecoverable) => {}
            Err(StorageError::ReadQuorum) => {}
            other => panic!("expected unrecoverable, got {:?}", other),
        }
    }

    #[test]
    fn heal_missing_two_shards_repairs() {
        use crate::storage::metadata::EcConfig;
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        put_test_object(&set, "test", "key", b"two missing shards");
        // upgrade bucket to FTT=2 (2+2) so we can test 2-disk failure
        set.set_ec_config("test", &EcConfig { ftt: 2 }).unwrap();
        // rewrite with FTT=2
        set.put_object("test", "key", b"two missing shards", PutOptions {
            content_type: "text/plain".to_string(),
            ec_ftt: Some(2),
            ..Default::default()
        }).unwrap();

        // delete 2 of 4 disk shards (exactly at parity limit)
        for i in 0..2 {
            let obj_dir = paths[i].join("test").join("key");
            if obj_dir.exists() {
                std::fs::remove_dir_all(&obj_dir).unwrap();
            }
        }

        let disks = make_backends(&paths);
        let result = heal_object(&disks, "test", "key").unwrap();
        assert!(matches!(result, HealResult::Repaired { shards_fixed } if shards_fixed == 2));

        // verify full integrity
        let (data, _) = set.get_object("test", "key").unwrap();
        assert_eq!(data, b"two missing shards");
    }

    #[test]
    fn heal_nonexistent_object_returns_not_found() {
        let (_base, paths) = make_disk_dirs(4);
        let _set = make_set(&paths);

        let disks = make_backends(&paths);
        let result = heal_object(&disks, "test", "nope");
        assert!(result.is_err());
    }

    #[test]
    fn object_needs_healing_detects_missing_shard() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        put_test_object(&set, "test", "key", b"check healing");

        let disks = make_backends(&paths);
        assert!(!object_needs_healing(&disks, "test", "key"));

        // delete one shard
        let obj_dir = paths[0].join("test").join("key");
        if obj_dir.exists() {
            std::fs::remove_dir_all(&obj_dir).unwrap();
        }

        assert!(object_needs_healing(&disks, "test", "key"));
    }

    #[test]
    fn heal_single_disk_no_parity() {
        let (_base, paths) = make_disk_dirs(1);
        let set = make_set(&paths);
        put_test_object(&set, "test", "key", b"no parity");

        let disks = make_backends(&paths);
        let result = heal_object(&disks, "test", "key").unwrap();
        assert!(matches!(result, HealResult::Healthy));
    }
}
