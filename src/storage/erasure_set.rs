use std::sync::Arc;

use super::Backend;
use super::erasure_decode::read_and_decode;
use super::erasure_encode::encode_and_write_with_mrf;
use super::metadata::{BucketInfo, ListOptions, ListResult, ObjectInfo, PutOptions};
use super::{StorageError, Store};
use crate::heal::mrf::MrfQueue;

pub struct ErasureSet {
    disks: Vec<Box<dyn Backend>>,
    data_n: usize,
    parity_n: usize,
    mrf: Option<Arc<MrfQueue>>,
}

impl ErasureSet {
    pub fn new(
        disks: Vec<Box<dyn Backend>>,
        data_n: usize,
        parity_n: usize,
    ) -> Result<Self, StorageError> {
        let total = data_n + parity_n;
        if disks.len() != total {
            return Err(StorageError::InvalidConfig(format!(
                "need {} disks (data={} + parity={}), got {}",
                total,
                data_n,
                parity_n,
                disks.len()
            )));
        }
        if data_n == 0 {
            return Err(StorageError::InvalidConfig(
                "data shards must be >= 1".to_string(),
            ));
        }
        Ok(Self {
            disks,
            data_n,
            parity_n,
            mrf: None,
        })
    }

    pub fn disks(&self) -> &[Box<dyn Backend>] {
        &self.disks
    }

    pub fn data_n(&self) -> usize {
        self.data_n
    }

    pub fn parity_n(&self) -> usize {
        self.parity_n
    }

    pub fn set_mrf(&mut self, mrf: Arc<MrfQueue>) {
        self.mrf = Some(mrf);
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
        // verify bucket exists on quorum of disks
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        encode_and_write_with_mrf(
            &self.disks,
            self.data_n,
            self.parity_n,
            bucket,
            key,
            data,
            opts,
            self.mrf.as_ref(),
        )
    }

    fn get_object(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectInfo), StorageError> {
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        let (data, meta) = read_and_decode(&self.disks, self.data_n, self.parity_n, bucket, key)?;
        Ok((
            data,
            ObjectInfo {
                bucket: bucket.to_string(),
                key: key.to_string(),
                size: meta.size,
                etag: meta.etag,
                content_type: meta.content_type,
                created_at: meta.created_at,
                user_metadata: meta.user_metadata,
                tags: meta.tags,
            },
        ))
    }

    fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectInfo, StorageError> {
        // read meta from first responsive disk
        for disk in &self.disks {
            match disk.stat_object(bucket, key) {
                Ok(meta) => {
                    return Ok(ObjectInfo {
                        bucket: bucket.to_string(),
                        key: key.to_string(),
                        size: meta.size,
                        etag: meta.etag,
                        content_type: meta.content_type,
                        created_at: meta.created_at,
                        user_metadata: meta.user_metadata,
                        tags: meta.tags,
                    });
                }
                Err(_) => continue,
            }
        }
        Err(StorageError::ObjectNotFound)
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        let mut successes = 0;
        let mut found = false;
        for disk in &self.disks {
            match disk.delete_object(bucket, key) {
                Ok(()) => {
                    successes += 1;
                    found = true;
                }
                Err(StorageError::ObjectNotFound) => {
                    // might not exist on this disk (partial write), still count
                    successes += 1;
                }
                Err(_) => {}
            }
        }
        if !found {
            return Err(StorageError::ObjectNotFound);
        }
        let delete_quorum = if self.parity_n == 0 {
            self.data_n
        } else {
            self.data_n + 1
        };
        if successes < delete_quorum {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    fn make_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        // check if already exists
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
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        // check if bucket has objects (from any disk)
        for disk in &self.disks {
            if let Ok(keys) = disk.list_objects(bucket, "")
                && !keys.is_empty()
            {
                return Err(StorageError::BucketNotEmpty);
            }
        }
        // delete from all disks
        for disk in &self.disks {
            let _ = disk.delete_bucket(bucket);
        }
        Ok(())
    }

    fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        let count = self
            .disks
            .iter()
            .filter(|d| d.bucket_exists(bucket))
            .count();
        Ok(count >= self.data_n)
    }

    fn list_buckets(&self) -> Result<Vec<BucketInfo>, StorageError> {
        // list from first responsive disk
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
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }

        // list keys from first responsive disk
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

        // apply delimiter grouping
        let mut objects = Vec::new();
        let mut common_prefixes = Vec::new();

        if opts.delimiter.is_empty() {
            // no grouping, just list all matching objects
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
        if !self.head_bucket(bucket)? {
            return Err(StorageError::BucketNotFound);
        }
        let mut successes = 0;
        let mut found = false;
        for disk in &self.disks {
            match disk.stat_object(bucket, key) {
                Ok(mut meta) => {
                    found = true;
                    meta.tags = tags.clone();
                    if disk.update_meta(bucket, key, &meta).is_ok() {
                        successes += 1;
                    }
                }
                Err(_) => continue,
            }
        }
        if !found {
            return Err(StorageError::ObjectNotFound);
        }
        let write_quorum = if self.parity_n == 0 {
            self.data_n
        } else {
            self.data_n + 1
        };
        if successes < write_quorum {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        self.put_object_tags(bucket, key, std::collections::HashMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::disk::LocalDisk;
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
            .map(|p| Box::new(LocalDisk::new(p.as_path()).unwrap()) as Box<dyn Backend>)
            .collect()
    }

    fn make_set(paths: &[std::path::PathBuf], data: usize, parity: usize) -> ErasureSet {
        ErasureSet::new(make_backends(paths), data, parity).unwrap()
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
    fn new_mismatched_disk_count() {
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

            // verify etag is md5 of original data
            assert_eq!(info.etag, crate::storage::bitrot::md5_hex(payload));

            // verify get returns original data
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

            // delete up to parity disk shard dirs
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

            // delete parity + 1 disk shards
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

        // corrupt shard.dat on disk 0
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

        // corrupt shard.dat on 3 of 4 disks
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

        // verify gone from all disks
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

        // remove ALL disk root directories to guarantee all writes fail
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
