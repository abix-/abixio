use std::sync::{Arc, RwLock};

use super::Backend;
use super::erasure_decode::{read_and_decode, read_and_decode_versioned};
use super::erasure_encode::{encode_and_write_versioned, encode_and_write_with_mrf};
use super::metadata::{
    BucketInfo, BucketSettings, EcConfig, ListOptions, ListResult, ObjectInfo, ObjectMeta,
    PutOptions, VersioningConfig,
};
use super::pathing;
use super::{StorageError, Store};
use crate::cluster::placement::{PlacementVolume, PlacementPlanner};
use crate::heal::mrf::MrfQueue;

/// Convert a failures-to-tolerate (FTT) count into (data, parity) shards.
/// Maximizes data shards for space efficiency: data = disks - ftt, parity = ftt.
pub fn ftt_to_ec(ftt: usize, num_disks: usize) -> Result<(usize, usize), StorageError> {
    if num_disks == 0 || ftt >= num_disks {
        return Err(StorageError::InvalidConfig(format!(
            "ftt {} requires at least {} disks, have {}",
            ftt,
            ftt + 1,
            num_disks
        )));
    }
    Ok((num_disks - ftt, ftt))
}

/// Auto-compute server default EC from volume count.
/// Default is FTT=1 (survive 1 failure) when possible.
pub fn default_ec(num_volumes: usize) -> (usize, usize) {
    let ftt = 1.min(num_volumes.saturating_sub(1));
    ftt_to_ec(ftt, num_volumes).unwrap_or((1, 0))
}

#[derive(Debug, Clone)]
struct PlacementTopology {
    epoch_id: u64,
    set_id: String,
    disks: Vec<PlacementVolume>,
}

pub struct ErasureSet {
    disks: Vec<Box<dyn Backend>>,
    default_data: usize,
    default_parity: usize,
    mrf: Option<Arc<MrfQueue>>,
    placement: RwLock<PlacementTopology>,
}

impl ErasureSet {
    pub fn new(
        disks: Vec<Box<dyn Backend>>,
        default_data: usize,
        default_parity: usize,
    ) -> Result<Self, StorageError> {
        if default_data == 0 {
            return Err(StorageError::InvalidConfig(
                "data shards must be >= 1".to_string(),
            ));
        }
        let total = default_data + default_parity;
        if disks.len() < total {
            return Err(StorageError::InvalidConfig(format!(
                "need at least {} disks (data={} + parity={}), got {}",
                total,
                default_data,
                default_parity,
                disks.len()
            )));
        }
        Ok(Self {
            placement: RwLock::new(PlacementTopology {
                epoch_id: 1,
                set_id: "local-set".to_string(),
                disks: (0..disks.len())
                    .map(|backend_index| PlacementVolume {
                        backend_index,
                        node_id: "local".to_string(),
                        volume_id: format!("vol-{}", backend_index),
                    })
                    .collect(),
            }),
            disks,
            default_data,
            default_parity,
            mrf: None,
        })
    }

    pub fn disks(&self) -> &[Box<dyn Backend>] {
        &self.disks
    }

    pub fn data_n(&self) -> usize {
        self.default_data
    }

    pub fn parity_n(&self) -> usize {
        self.default_parity
    }

    pub fn set_mrf(&mut self, mrf: Arc<MrfQueue>) {
        self.mrf = Some(mrf);
    }

    pub fn set_placement_topology(
        &self,
        epoch_id: u64,
        set_id: impl Into<String>,
        disks: Vec<PlacementVolume>,
    ) -> Result<(), StorageError> {
        if disks.len() != self.disks.len() {
            return Err(StorageError::InvalidConfig(format!(
                "placement disk count mismatch: expected {}, got {}",
                self.disks.len(),
                disks.len()
            )));
        }
        let mut guard = self.placement.write().unwrap();
        guard.epoch_id = epoch_id;
        guard.set_id = set_id.into();
        guard.disks = disks;
        Ok(())
    }

    pub fn placement_planner(&self) -> PlacementPlanner {
        let guard = self.placement.read().unwrap();
        PlacementPlanner::new(guard.epoch_id, guard.set_id.clone(), guard.disks.clone())
    }

    pub fn placement_snapshot(&self) -> (u64, String, Vec<PlacementVolume>) {
        let guard = self.placement.read().unwrap();
        (guard.epoch_id, guard.set_id.clone(), guard.disks.clone())
    }

    /// Resolve EC params for a write operation.
    /// Precedence: per-object raw > per-object FTT > bucket raw > bucket FTT > server defaults.
    fn resolve_ec(&self, bucket: &str, opts: &PutOptions) -> (usize, usize) {
        // per-object raw override from PutOptions
        if let (Some(d), Some(p)) = (opts.ec_data, opts.ec_parity)
            && d >= 1
            && d + p <= self.disks.len()
        {
            return (d, p);
        }

        // per-object FTT
        if let Some(ftt) = opts.ec_ftt {
            if let Ok((d, p)) = ftt_to_ec(ftt, self.disks.len()) {
                return (d, p);
            }
        }

        // bucket-level config (FTT configs store computed data/parity alongside ftt)
        if let Some(ec) = self
            .disks
            .iter()
            .find_map(|d| d.read_bucket_settings(bucket).ec)
            && ec.data >= 1
            && ec.data + ec.parity <= self.disks.len()
        {
            return (ec.data, ec.parity);
        }

        // server defaults
        (self.default_data, self.default_parity)
    }

    /// Read meta from any available disk to get the object's stored EC params.
    fn read_ec_from_meta(&self, bucket: &str, key: &str) -> Option<(usize, usize)> {
        for disk in &self.disks {
            if let Ok(meta) = disk.stat_object(bucket, key) {
                return Some((meta.erasure.data, meta.erasure.parity));
            }
        }
        None
    }

    fn meta_to_info(bucket: &str, key: &str, meta: &ObjectMeta) -> ObjectInfo {
        ObjectInfo {
            bucket: bucket.to_string(),
            key: key.to_string(),
            size: meta.size,
            etag: meta.etag.clone(),
            content_type: meta.content_type.clone(),
            created_at: meta.created_at,
            user_metadata: meta.user_metadata.clone(),
            tags: meta.tags.clone(),
            version_id: meta.version_id.clone(),
            is_delete_marker: meta.is_delete_marker,
        }
    }
}

impl Store for ErasureSet {
    fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        opts: PutOptions,
    ) -> Result<ObjectInfo, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        let (data_n, parity_n) = self.resolve_ec(bucket, &opts);
        let planner = self.placement_planner();
        encode_and_write_with_mrf(
            &self.disks,
            &planner,
            data_n,
            parity_n,
            bucket,
            key,
            data,
            opts,
            self.mrf.as_ref(),
        )
    }

    fn get_object(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectInfo), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        let (data_n, parity_n) = self
            .read_ec_from_meta(bucket, key)
            .unwrap_or((self.default_data, self.default_parity));
        let (data, meta) = read_and_decode(&self.disks, data_n, parity_n, bucket, key)?;
        Ok((data, Self::meta_to_info(bucket, key, &meta)))
    }

    fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectInfo, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in &self.disks {
            if let Ok(meta) = disk.stat_object(bucket, key) {
                return Ok(Self::meta_to_info(bucket, key, &meta));
            }
        }
        Err(StorageError::ObjectNotFound)
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        // get stored EC params for quorum calculation
        let (data_n, parity_n) = self
            .read_ec_from_meta(bucket, key)
            .unwrap_or((self.default_data, self.default_parity));

        let mut successes = 0;
        let mut found = false;
        for disk in &self.disks {
            match disk.delete_object(bucket, key) {
                Ok(()) => {
                    successes += 1;
                    found = true;
                }
                Err(StorageError::ObjectNotFound) => {
                    successes += 1;
                }
                Err(_) => {}
            }
        }
        if !found {
            return Err(StorageError::ObjectNotFound);
        }
        let delete_quorum = if parity_n == 0 { data_n } else { data_n + 1 };
        if successes < delete_quorum {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    fn make_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if self.disks.iter().any(|d| d.bucket_exists(bucket)) {
            return Err(StorageError::BucketExists);
        }
        let mut successes = 0;
        for disk in &self.disks {
            if disk.make_bucket(bucket).is_ok() {
                successes += 1;
            }
        }
        if successes == 0 {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in &self.disks {
            if let Ok(keys) = disk.list_objects(bucket, "")
                && !keys.is_empty()
            {
                return Err(StorageError::BucketNotEmpty);
            }
        }
        for disk in &self.disks {
            let _ = disk.delete_bucket(bucket);
        }
        Ok(())
    }

    fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        let count = self
            .disks
            .iter()
            .filter(|d| d.bucket_exists(bucket))
            .count();
        // bucket must exist on at least 1 disk
        Ok(count >= 1)
    }

    fn list_buckets(&self) -> Result<Vec<BucketInfo>, StorageError> {
        for disk in &self.disks {
            match disk.list_buckets() {
                Ok(names) => {
                    return Ok(names
                        .into_iter()
                        .map(|name| {
                            let created_at = disk.bucket_created_at(&name);
                            BucketInfo { name, created_at }
                        })
                        .collect());
                }
                Err(_) => continue,
            }
        }
        Ok(Vec::new())
    }

    fn list_objects(&self, bucket: &str, opts: ListOptions) -> Result<ListResult, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_prefix(&opts.prefix)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }

        let mut keys = Vec::new();
        for disk in &self.disks {
            match disk.list_objects(bucket, &opts.prefix) {
                Ok(k) => {
                    keys = k;
                    break;
                }
                Err(_) => continue,
            }
        }

        let mut objects = Vec::new();
        let mut common_prefixes = Vec::new();

        if opts.delimiter.is_empty() {
            for key in &keys {
                if let Ok(info) = self.head_object(bucket, key) {
                    objects.push(info);
                }
            }
        } else {
            let mut seen_prefixes = std::collections::HashSet::new();
            for key in &keys {
                let after_prefix = &key[opts.prefix.len()..];
                if let Some(pos) = after_prefix.find(&opts.delimiter) {
                    let cp = format!("{}{}", opts.prefix, &after_prefix[..=pos]);
                    if seen_prefixes.insert(cp.clone()) {
                        common_prefixes.push(cp);
                    }
                } else if let Ok(info) = self.head_object(bucket, key) {
                    objects.push(info);
                }
            }
        }

        let max_keys = if opts.max_keys == 0 {
            1000
        } else {
            opts.max_keys
        };
        let is_truncated = objects.len() + common_prefixes.len() > max_keys;

        Ok(ListResult {
            objects,
            common_prefixes,
            is_truncated,
            next_continuation_token: String::new(),
        })
    }

    fn get_object_tags(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<std::collections::HashMap<String, String>, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in &self.disks {
            match disk.stat_object(bucket, key) {
                Ok(meta) => return Ok(meta.tags),
                Err(_) => continue,
            }
        }
        Err(StorageError::ObjectNotFound)
    }

    fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        tags: std::collections::HashMap<String, String>,
    ) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }

        let (data_n, parity_n) = self
            .read_ec_from_meta(bucket, key)
            .unwrap_or((self.default_data, self.default_parity));

        let mut successes = 0;
        let mut found = false;
        for disk in &self.disks {
            let mut versions = disk.read_meta_versions(bucket, key).unwrap_or_default();
            if let Some(latest) = versions.iter_mut().find(|v| !v.is_delete_marker) {
                found = true;
                latest.tags = tags.clone();
                if disk.write_meta_versions(bucket, key, &versions).is_ok() {
                    successes += 1;
                }
            }
        }
        if !found {
            return Err(StorageError::ObjectNotFound);
        }
        let write_quorum = if parity_n == 0 { data_n } else { data_n + 1 };
        if successes < write_quorum {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        self.put_object_tags(bucket, key, std::collections::HashMap::new())
    }

    // -- versioning --

    fn put_object_versioned(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        opts: PutOptions,
        version_id: &str,
    ) -> Result<ObjectInfo, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        pathing::validate_version_id(version_id)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        let (data_n, parity_n) = self.resolve_ec(bucket, &opts);
        let planner = self.placement_planner();
        encode_and_write_versioned(
            &self.disks,
            &planner,
            data_n,
            parity_n,
            bucket,
            key,
            data,
            opts,
            self.mrf.as_ref(),
            version_id,
        )
    }

    fn get_versioning_config(
        &self,
        bucket: &str,
    ) -> Result<Option<VersioningConfig>, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in &self.disks {
            let settings = disk.read_bucket_settings(bucket);
            if let Some(status) = settings.versioning {
                return Ok(Some(VersioningConfig { status }));
            }
        }
        Ok(None)
    }

    fn set_versioning_config(
        &self,
        bucket: &str,
        config: &VersioningConfig,
    ) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        let mut successes = 0;
        for disk in &self.disks {
            let mut settings = disk.read_bucket_settings(bucket);
            settings.versioning = Some(config.status.clone());
            if disk.write_bucket_settings(bucket, &settings).is_ok() {
                successes += 1;
            }
        }
        if successes == 0 {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectInfo), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        pathing::validate_version_id(version_id)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        // for versioned reads, get EC from meta (may differ from defaults)
        let (data_n, parity_n) = self
            .read_ec_from_meta(bucket, key)
            .unwrap_or((self.default_data, self.default_parity));
        let (data, meta) =
            read_and_decode_versioned(&self.disks, data_n, parity_n, bucket, key, version_id)?;
        Ok((data, Self::meta_to_info(bucket, key, &meta)))
    }

    fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        pathing::validate_version_id(version_id)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in &self.disks {
            let _ = disk.delete_version_data(bucket, key, version_id);
        }
        Ok(())
    }

    fn list_object_versions(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, Vec<ObjectMeta>)>, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_prefix(prefix)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        let mut keys = Vec::new();
        for disk in &self.disks {
            match disk.list_objects(bucket, prefix) {
                Ok(k) => {
                    keys = k;
                    break;
                }
                Err(_) => continue,
            }
        }
        let mut result = Vec::new();
        for key in &keys {
            for disk in &self.disks {
                let versions = disk.read_meta_versions(bucket, key).unwrap_or_default();
                if !versions.is_empty() {
                    result.push((key.clone(), versions));
                    break;
                }
            }
        }
        Ok(result)
    }

    // -- per-bucket EC config --

    fn get_ec_config(&self, bucket: &str) -> Result<Option<EcConfig>, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in &self.disks {
            let settings = disk.read_bucket_settings(bucket);
            if settings.ec.is_some() {
                return Ok(settings.ec);
            }
        }
        Ok(None)
    }

    fn set_ec_config(&self, bucket: &str, config: &EcConfig) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        if config.data == 0 {
            return Err(StorageError::InvalidConfig(
                "ec data shards must be >= 1".to_string(),
            ));
        }
        if config.data + config.parity > self.disks.len() {
            return Err(StorageError::InvalidConfig(format!(
                "ec data({}) + parity({}) exceeds disk count({})",
                config.data,
                config.parity,
                self.disks.len()
            )));
        }
        let mut successes = 0;
        for disk in &self.disks {
            let mut settings = disk.read_bucket_settings(bucket);
            settings.ec = Some(config.clone());
            if disk.write_bucket_settings(bucket, &settings).is_ok() {
                successes += 1;
            }
        }
        if successes == 0 {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    fn get_bucket_settings(
        &self,
        bucket: &str,
    ) -> Result<BucketSettings, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        // read from first available disk
        for disk in &self.disks {
            let settings = disk.read_bucket_settings(bucket);
            if settings != BucketSettings::default() {
                return Ok(settings);
            }
        }
        Ok(BucketSettings::default())
    }

    fn set_bucket_settings(
        &self,
        bucket: &str,
        settings: &BucketSettings,
    ) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        let mut successes = 0;
        for disk in &self.disks {
            if disk.write_bucket_settings(bucket, settings).is_ok() {
                successes += 1;
            }
        }
        if successes == 0 {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    fn disk_count(&self) -> usize {
        self.disks.len()
    }

    fn default_data(&self) -> usize {
        self.default_data
    }

    fn default_parity(&self) -> usize {
        self.default_parity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::local_volume::LocalVolume;
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
            .map(|p| Box::new(LocalVolume::new(p.as_path()).unwrap()) as Box<dyn Backend>)
            .collect()
    }

    fn make_set(paths: &[std::path::PathBuf], data: usize, parity: usize) -> ErasureSet {
        ErasureSet::new(make_backends(paths), data, parity).unwrap()
    }

    // -- default_ec tests --

    #[test]
    fn default_ec_single_volume() {
        assert_eq!(default_ec(1), (1, 0));
    }

    #[test]
    fn default_ec_two_volumes() {
        assert_eq!(default_ec(2), (1, 1));
    }

    #[test]
    fn default_ec_four_volumes() {
        assert_eq!(default_ec(4), (3, 1));
    }

    #[test]
    fn default_ec_eight_volumes() {
        assert_eq!(default_ec(8), (7, 1));
    }

    // -- construction tests --

    #[test]
    fn new_4_disks_2_2() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        assert_eq!(set.data_n(), 2);
        assert_eq!(set.parity_n(), 2);
    }

    #[test]
    fn new_1_disk_1_0() {
        let (_base, paths) = make_disk_dirs(1);
        let set = make_set(&paths, 1, 0);
        assert_eq!(set.data_n(), 1);
        assert_eq!(set.parity_n(), 0);
    }

    #[test]
    fn new_more_disks_than_needed() {
        // pool with 6 disks, default EC 2+2 -- should work now
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths, 2, 2);
        assert_eq!(set.disks().len(), 6);
        assert_eq!(set.data_n(), 2);
        assert_eq!(set.parity_n(), 2);
    }

    #[test]
    fn new_too_few_disks_fails() {
        let (_base, paths) = make_disk_dirs(3);
        let backends = make_backends(&paths);
        assert!(ErasureSet::new(backends, 2, 2).is_err());
    }

    // -- put + get round-trip across configs --

    struct TestConfig {
        data: usize,
        parity: usize,
    }

    const CONFIGS: &[TestConfig] = &[
        TestConfig { data: 1, parity: 0 },
        TestConfig { data: 1, parity: 1 },
        TestConfig { data: 2, parity: 1 },
        TestConfig { data: 2, parity: 2 },
        TestConfig { data: 3, parity: 3 },
    ];

    #[test]
    fn put_get_round_trip_all_configs() {
        for cfg in CONFIGS {
            let total = cfg.data + cfg.parity;
            let (_base, paths) = make_disk_dirs(total);
            let set = make_set(&paths, cfg.data, cfg.parity);
            set.make_bucket("test").unwrap();

            let payload = b"the quick brown fox jumps over the lazy dog";
            let opts = PutOptions {
                content_type: "text/plain".to_string(),
                ..Default::default()
            };
            let info = set.put_object("test", "fox.txt", payload, opts).unwrap();

            assert_eq!(info.etag, crate::storage::bitrot::md5_hex(payload));

            let (data, get_info) = set.get_object("test", "fox.txt").unwrap();
            assert_eq!(
                data, payload,
                "data mismatch for config data={} parity={}",
                cfg.data, cfg.parity
            );
            assert_eq!(get_info.size, payload.len() as u64);
            assert_eq!(get_info.content_type, "text/plain");
            assert_eq!(get_info.etag, info.etag);
        }
    }

    #[test]
    fn head_object_returns_correct_info() {
        for cfg in CONFIGS {
            let total = cfg.data + cfg.parity;
            let (_base, paths) = make_disk_dirs(total);
            let set = make_set(&paths, cfg.data, cfg.parity);
            set.make_bucket("test").unwrap();

            let payload = b"head test data";
            let opts = PutOptions {
                content_type: "application/json".to_string(),
                ..Default::default()
            };
            set.put_object("test", "obj", payload, opts).unwrap();

            let info = set.head_object("test", "obj").unwrap();
            assert_eq!(info.size, payload.len() as u64);
            assert_eq!(info.content_type, "application/json");
        }
    }

    // -- per-object EC in a larger pool --

    #[test]
    fn per_object_ec_override() {
        // 6-disk pool with default EC 2+2
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();

        // write with per-object EC 1+1 (uses only 2 of 6 disks)
        let payload = b"per-object ec test";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ec_data: Some(1),
            ec_parity: Some(1),
            ..Default::default()
        };
        set.put_object("test", "small", payload, opts).unwrap();

        // read back -- uses stored EC from meta
        let (data, _) = set.get_object("test", "small").unwrap();
        assert_eq!(data, payload);

        // write with default EC (2+2, uses 4 of 6 disks)
        let opts2 = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        let payload2 = b"default ec test";
        set.put_object("test", "normal", payload2, opts2).unwrap();
        let (data2, _) = set.get_object("test", "normal").unwrap();
        assert_eq!(data2, payload2);
    }

    #[test]
    fn per_object_ec_max_parity() {
        // 6-disk pool, write with EC 1+5 (max parity)
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths, 1, 1);
        set.make_bucket("test").unwrap();

        let payload = b"max parity test data";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ec_data: Some(1),
            ec_parity: Some(5),
            ..Default::default()
        };
        set.put_object("test", "critical", payload, opts).unwrap();

        // delete 4 of 6 disks' object data -- should still be readable (parity=5)
        for i in 0..4 {
            let obj_dir = paths[i].join("test").join("critical");
            if obj_dir.exists() {
                std::fs::remove_dir_all(&obj_dir).unwrap();
            }
        }

        let (data, _) = set.get_object("test", "critical").unwrap();
        assert_eq!(data, payload);
    }

    #[test]
    fn bucket_ec_config() {
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();

        // no config initially
        assert!(set.get_ec_config("test").unwrap().is_none());

        // set bucket EC config
        let config = EcConfig { data: 3, parity: 3, ftt: None };
        set.set_ec_config("test", &config).unwrap();

        // verify it's stored
        let loaded = set.get_ec_config("test").unwrap().unwrap();
        assert_eq!(loaded, config);

        // write object -- should use bucket config (3+3)
        let payload = b"bucket ec test";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "key", payload, opts).unwrap();

        // verify the object used 3+3
        let meta = set.read_ec_from_meta("test", "key").unwrap();
        assert_eq!(meta, (3, 3));
    }

    #[test]
    fn per_object_overrides_bucket_config() {
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();

        // set bucket config to 3+3
        set.set_ec_config("test", &EcConfig { data: 3, parity: 3, ftt: None })
            .unwrap();

        // write with per-object override 1+1
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ec_data: Some(1),
            ec_parity: Some(1),
            ..Default::default()
        };
        set.put_object("test", "key", b"override", opts).unwrap();

        // verify per-object won
        let meta = set.read_ec_from_meta("test", "key").unwrap();
        assert_eq!(meta, (1, 1));
    }

    #[test]
    fn ec_config_validation() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();

        // data=0 should fail
        assert!(
            set.set_ec_config("test", &EcConfig { data: 0, parity: 2, ftt: None })
                .is_err()
        );

        // exceeds disk count should fail
        assert!(
            set.set_ec_config("test", &EcConfig { data: 3, parity: 3, ftt: None })
                .is_err()
        );

        // valid config should succeed
        assert!(
            set.set_ec_config("test", &EcConfig { data: 2, parity: 2, ftt: None })
                .is_ok()
        );
    }

    // -- FTT mapping --

    #[test]
    fn ftt_to_ec_mapping() {
        // 1 disk
        assert_eq!(ftt_to_ec(0, 1).unwrap(), (1, 0));
        assert!(ftt_to_ec(1, 1).is_err());

        // 2 disks
        assert_eq!(ftt_to_ec(0, 2).unwrap(), (2, 0));
        assert_eq!(ftt_to_ec(1, 2).unwrap(), (1, 1));
        assert!(ftt_to_ec(2, 2).is_err());

        // 4 disks
        assert_eq!(ftt_to_ec(0, 4).unwrap(), (4, 0));
        assert_eq!(ftt_to_ec(1, 4).unwrap(), (3, 1));
        assert_eq!(ftt_to_ec(2, 4).unwrap(), (2, 2));
        assert_eq!(ftt_to_ec(3, 4).unwrap(), (1, 3));
        assert!(ftt_to_ec(4, 4).is_err());

        // 6 disks
        assert_eq!(ftt_to_ec(0, 6).unwrap(), (6, 0));
        assert_eq!(ftt_to_ec(1, 6).unwrap(), (5, 1));
        assert_eq!(ftt_to_ec(2, 6).unwrap(), (4, 2));
        assert_eq!(ftt_to_ec(3, 6).unwrap(), (3, 3));

        // 0 disks
        assert!(ftt_to_ec(0, 0).is_err());
    }

    #[test]
    fn default_ec_unchanged() {
        assert_eq!(default_ec(1), (1, 0));
        assert_eq!(default_ec(2), (1, 1));
        assert_eq!(default_ec(4), (3, 1));
        assert_eq!(default_ec(6), (5, 1));
    }

    #[test]
    fn resolve_ec_ftt_per_object() {
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths, 5, 1);
        set.make_bucket("test").unwrap();

        // FTT=2 on 6 disks should give 4+2
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ec_ftt: Some(2),
            ..Default::default()
        };
        set.put_object("test", "ftt2", b"ftt test", opts).unwrap();
        let meta = set.read_ec_from_meta("test", "ftt2").unwrap();
        assert_eq!(meta, (4, 2));
    }

    #[test]
    fn resolve_ec_raw_overrides_ftt() {
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths, 5, 1);
        set.make_bucket("test").unwrap();

        // raw data/parity should win over FTT
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ec_data: Some(1),
            ec_parity: Some(1),
            ec_ftt: Some(3),
            ..Default::default()
        };
        set.put_object("test", "raw-wins", b"raw wins", opts).unwrap();
        let meta = set.read_ec_from_meta("test", "raw-wins").unwrap();
        assert_eq!(meta, (1, 1));
    }

    #[test]
    fn resolve_ec_bucket_ftt() {
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths, 5, 1);
        set.make_bucket("test").unwrap();

        // set bucket config with FTT=2 on 6 disks -> 4+2
        let (data, parity) = ftt_to_ec(2, 6).unwrap();
        let config = EcConfig { data, parity, ftt: Some(2) };
        set.set_ec_config("test", &config).unwrap();

        // write with no per-object EC -- should use bucket config 4+2
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "bucket-ftt", b"bucket ftt", opts).unwrap();
        let meta = set.read_ec_from_meta("test", "bucket-ftt").unwrap();
        assert_eq!(meta, (4, 2));
    }

    // -- erasure resilience --

    #[test]
    fn resilience_survives_parity_failures() {
        for cfg in CONFIGS.iter().filter(|c| c.parity > 0) {
            let total = cfg.data + cfg.parity;
            let (_base, paths) = make_disk_dirs(total);
            let set = make_set(&paths, cfg.data, cfg.parity);
            set.make_bucket("test").unwrap();

            let payload = b"resilience test data that should survive disk failures";
            let opts = PutOptions {
                content_type: "text/plain".to_string(),
                ..Default::default()
            };
            set.put_object("test", "key", payload, opts).unwrap();

            for i in 0..cfg.parity {
                let obj_dir = paths[total - 1 - i].join("test").join("key");
                if obj_dir.exists() {
                    std::fs::remove_dir_all(&obj_dir).unwrap();
                }
            }

            let (data, _) = set.get_object("test", "key").unwrap();
            assert_eq!(
                data, payload,
                "failed for config data={} parity={}",
                cfg.data, cfg.parity
            );
        }
    }

    #[test]
    fn resilience_fails_beyond_parity() {
        for cfg in CONFIGS.iter().filter(|c| c.parity > 0) {
            let total = cfg.data + cfg.parity;
            let (_base, paths) = make_disk_dirs(total);
            let set = make_set(&paths, cfg.data, cfg.parity);
            set.make_bucket("test").unwrap();

            let payload = b"this should fail";
            let opts = PutOptions {
                content_type: "text/plain".to_string(),
                ..Default::default()
            };
            set.put_object("test", "key", payload, opts).unwrap();

            for i in 0..(cfg.parity + 1) {
                let obj_dir = paths[total - 1 - i].join("test").join("key");
                if obj_dir.exists() {
                    std::fs::remove_dir_all(&obj_dir).unwrap();
                }
            }

            let result = set.get_object("test", "key");
            assert!(
                matches!(
                    result,
                    Err(StorageError::ReadQuorum) | Err(StorageError::ObjectNotFound)
                ),
                "should fail for config data={} parity={}",
                cfg.data,
                cfg.parity
            );
        }
    }

    // -- bitrot detection --

    #[test]
    fn bitrot_one_corrupt_shard_recovers() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();

        let payload = b"bitrot test data";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "key", payload, opts).unwrap();

        let shard_path = paths[0].join("test").join("key").join("shard.dat");
        if shard_path.exists() {
            std::fs::write(&shard_path, b"CORRUPTED").unwrap();
        }

        let (data, _) = set.get_object("test", "key").unwrap();
        assert_eq!(data, payload);
    }

    #[test]
    fn bitrot_too_many_corrupt_fails() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();

        let payload = b"bitrot fail test";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "key", payload, opts).unwrap();

        for i in 0..3 {
            let shard_path = paths[i].join("test").join("key").join("shard.dat");
            if shard_path.exists() {
                std::fs::write(&shard_path, b"CORRUPTED").unwrap();
            }
        }

        assert!(matches!(
            set.get_object("test", "key"),
            Err(StorageError::ReadQuorum)
        ));
    }

    // -- bucket operations --

    #[test]
    fn make_bucket_creates_on_all_disks() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("mybucket").unwrap();
        for path in &paths {
            assert!(path.join("mybucket").is_dir());
        }
    }

    #[test]
    fn head_bucket_true_after_create() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        assert!(!set.head_bucket("nope").unwrap());
        set.make_bucket("test").unwrap();
        assert!(set.head_bucket("test").unwrap());
    }

    #[test]
    fn list_buckets_returns_all() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("alpha").unwrap();
        set.make_bucket("beta").unwrap();
        let buckets = set.list_buckets().unwrap();
        let names: Vec<&str> = buckets.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn delete_object_removes_from_all_disks() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "key", b"data", opts).unwrap();
        set.delete_object("test", "key").unwrap();

        for path in &paths {
            assert!(!path.join("test").join("key").exists());
        }
    }

    // -- list operations --

    #[test]
    fn list_objects_returns_all() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "aaa", b"1", opts.clone()).unwrap();
        set.put_object("test", "bbb", b"2", opts.clone()).unwrap();
        set.put_object("test", "ccc", b"3", opts).unwrap();

        let result = set.list_objects("test", ListOptions::default()).unwrap();
        let keys: Vec<&str> = result.objects.iter().map(|o| o.key.as_str()).collect();
        assert!(keys.contains(&"aaa"));
        assert!(keys.contains(&"bbb"));
        assert!(keys.contains(&"ccc"));
    }

    #[test]
    fn list_objects_with_prefix() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "logs/a", b"1", opts.clone())
            .unwrap();
        set.put_object("test", "logs/b", b"2", opts.clone())
            .unwrap();
        set.put_object("test", "data/c", b"3", opts).unwrap();

        let result = set
            .list_objects(
                "test",
                ListOptions {
                    prefix: "logs/".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.objects.len(), 2);
    }

    #[test]
    fn list_objects_with_delimiter() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "a/1", b"1", opts.clone()).unwrap();
        set.put_object("test", "a/2", b"2", opts.clone()).unwrap();
        set.put_object("test", "b/3", b"3", opts.clone()).unwrap();
        set.put_object("test", "root", b"4", opts).unwrap();

        let result = set
            .list_objects(
                "test",
                ListOptions {
                    delimiter: "/".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(result.common_prefixes.contains(&"a/".to_string()));
        assert!(result.common_prefixes.contains(&"b/".to_string()));
        assert_eq!(result.objects.len(), 1); // just "root"
        assert_eq!(result.objects[0].key, "root");
    }

    // -- quorum enforcement --

    #[test]
    fn write_quorum_failure() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths, 2, 2);
        set.make_bucket("test").unwrap();

        for path in &paths {
            std::fs::remove_dir_all(path).unwrap();
        }

        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        let result = set.put_object("test", "key", b"data", opts);
        assert!(result.is_err());
    }

    #[test]
    fn single_disk_no_parity_write_fails_when_disk_gone() {
        let (_base, paths) = make_disk_dirs(1);
        let set = make_set(&paths, 1, 0);
        set.make_bucket("test").unwrap();

        std::fs::remove_dir_all(&paths[0]).unwrap();

        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        assert!(set.put_object("test", "key", b"data", opts).is_err());
    }
}
