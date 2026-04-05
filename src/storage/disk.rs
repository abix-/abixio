use std::fs;
use std::path::{Path, PathBuf};

use super::metadata::{
    EcConfig, ObjectMeta, ObjectMetaFile, VersioningConfig, read_meta_file, write_meta_file,
};
use super::{Backend, BackendInfo, StorageError};

pub struct LocalDisk {
    root: PathBuf,
}

const TMP_DIR: &str = ".abixio.tmp";
const SHARD_FILE: &str = "shard.dat";
const META_FILE: &str = "meta.json";
const VERSIONING_FILE: &str = ".versioning.json";
const EC_CONFIG_FILE: &str = ".ec.json";

impl LocalDisk {
    pub fn new(root: &Path) -> Result<Self, StorageError> {
        if !root.is_dir() {
            return Err(StorageError::InvalidConfig(format!(
                "disk path does not exist: {}",
                root.display()
            )));
        }
        // ensure tmp dir exists
        let tmp = root.join(TMP_DIR);
        fs::create_dir_all(&tmp)?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn walk_keys(
        &self,
        base: &Path,
        dir: &Path,
        prefix: &str,
        keys: &mut Vec<String>,
    ) -> Result<(), StorageError> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                // check if this dir is an object (has meta.json)
                if entry.path().join(META_FILE).exists() {
                    let rel = entry
                        .path()
                        .strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    if rel.starts_with(prefix) {
                        keys.push(rel);
                    }
                } else {
                    // recurse into subdirectory
                    self.walk_keys(base, &entry.path(), prefix, keys)?;
                }
            }
        }
        Ok(())
    }
}

impl Backend for LocalDisk {
    fn write_shard(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        fs::create_dir_all(&obj_dir)?;

        // write shard data directly to key/shard.dat
        fs::write(obj_dir.join(SHARD_FILE), data)?;

        // update meta.json: single version entry (unversioned = overwrite)
        let mut version = meta.clone();
        version.is_latest = true;
        let mf = ObjectMetaFile {
            versions: vec![version],
        };
        write_meta_file(&obj_dir.join(META_FILE), &mf).map_err(StorageError::Io)?;

        Ok(())
    }

    fn read_shard(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let mf =
            read_meta_file(&obj_dir.join(META_FILE)).map_err(|_| StorageError::ObjectNotFound)?;

        // find latest non-delete-marker version
        let version = mf
            .versions
            .iter()
            .find(|v| !v.is_delete_marker)
            .ok_or(StorageError::ObjectNotFound)?;

        // determine shard path: unversioned = key/shard.dat, versioned = key/<uuid>/shard.dat
        let shard_path = if version.version_id.is_empty() {
            obj_dir.join(SHARD_FILE)
        } else {
            obj_dir.join(&version.version_id).join(SHARD_FILE)
        };

        let data = fs::read(&shard_path).map_err(|_| StorageError::ObjectNotFound)?;
        Ok((data, version.clone()))
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        fs::remove_dir_all(&obj_dir)?;
        Ok(())
    }

    fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, StorageError> {
        let bucket_dir = self.root.join(bucket);
        if !bucket_dir.is_dir() {
            return Err(StorageError::BucketNotFound);
        }
        let mut keys = Vec::new();
        self.walk_keys(&bucket_dir, &bucket_dir, prefix, &mut keys)?;
        keys.sort();
        Ok(keys)
    }

    fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        let mut buckets = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if entry.file_type()?.is_dir() {
                buckets.push(name);
            }
        }
        buckets.sort();
        Ok(buckets)
    }

    fn make_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let path = self.root.join(bucket);
        if path.is_dir() {
            return Err(StorageError::BucketExists);
        }
        fs::create_dir_all(&path)?;
        Ok(())
    }

    fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let path = self.root.join(bucket);
        if !path.is_dir() {
            return Err(StorageError::BucketNotFound);
        }
        fs::remove_dir(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                || e.to_string().contains("not empty")
            {
                StorageError::BucketNotEmpty
            } else {
                StorageError::Io(e)
            }
        })
    }

    fn bucket_exists(&self, bucket: &str) -> bool {
        self.root.join(bucket).is_dir()
    }

    fn bucket_created_at(&self, bucket: &str) -> u64 {
        let path = self.root.join(bucket);
        fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn stat_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let mf =
            read_meta_file(&obj_dir.join(META_FILE)).map_err(|_| StorageError::ObjectNotFound)?;
        mf.versions
            .iter()
            .find(|v| !v.is_delete_marker)
            .cloned()
            .ok_or(StorageError::ObjectNotFound)
    }

    fn update_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        // replace the matching version entry (by index) in meta.json
        let mut mf =
            read_meta_file(&obj_dir.join(META_FILE)).map_err(|_| StorageError::ObjectNotFound)?;
        if let Some(v) = mf
            .versions
            .iter_mut()
            .find(|v| v.erasure.index == meta.erasure.index || v.version_id == meta.version_id)
        {
            *v = meta.clone();
        }
        write_meta_file(&obj_dir.join(META_FILE), &mf).map_err(StorageError::Io)
    }

    fn write_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        let ver_dir = obj_dir.join(version_id);
        fs::create_dir_all(&ver_dir)?;

        // write shard data to key/<uuid>/shard.dat
        fs::write(ver_dir.join(SHARD_FILE), data)?;

        // update meta.json: add new version entry at front, mark others as not latest
        let mut mf = read_meta_file(&obj_dir.join(META_FILE)).unwrap_or(ObjectMetaFile {
            versions: Vec::new(),
        });
        for v in &mut mf.versions {
            v.is_latest = false;
        }
        let mut version = meta.clone();
        version.is_latest = true;
        version.version_id = version_id.to_string();
        mf.versions.insert(0, version);
        write_meta_file(&obj_dir.join(META_FILE), &mf).map_err(StorageError::Io)?;

        Ok(())
    }

    fn read_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        let mf =
            read_meta_file(&obj_dir.join(META_FILE)).map_err(|_| StorageError::ObjectNotFound)?;

        let version = mf
            .versions
            .iter()
            .find(|v| v.version_id == version_id)
            .ok_or(StorageError::ObjectNotFound)?;

        let shard_path = obj_dir.join(version_id).join(SHARD_FILE);
        let data = fs::read(&shard_path).map_err(|_| StorageError::ObjectNotFound)?;
        Ok((data, version.clone()))
    }

    fn delete_version_data(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError> {
        let obj_dir = self.root.join(bucket).join(key);

        // remove shard data dir
        let ver_dir = obj_dir.join(version_id);
        if ver_dir.is_dir() {
            fs::remove_dir_all(&ver_dir)?;
        }

        // remove version entry from meta.json
        if let Ok(mut mf) = read_meta_file(&obj_dir.join(META_FILE)) {
            mf.versions.retain(|v| v.version_id != version_id);
            // update is_latest
            if let Some(first) = mf.versions.first_mut() {
                first.is_latest = true;
            }
            let _ = write_meta_file(&obj_dir.join(META_FILE), &mf);
        }
        Ok(())
    }

    fn read_meta_versions(&self, bucket: &str, key: &str) -> Result<Vec<ObjectMeta>, StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        match read_meta_file(&obj_dir.join(META_FILE)) {
            Ok(mf) => Ok(mf.versions),
            Err(_) => Ok(Vec::new()),
        }
    }

    fn write_meta_versions(
        &self,
        bucket: &str,
        key: &str,
        versions: &[ObjectMeta],
    ) -> Result<(), StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        fs::create_dir_all(&obj_dir)?;
        let mf = ObjectMetaFile {
            versions: versions.to_vec(),
        };
        write_meta_file(&obj_dir.join(META_FILE), &mf).map_err(StorageError::Io)
    }

    fn read_versioning_config(&self, bucket: &str) -> Option<VersioningConfig> {
        let path = self.root.join(bucket).join(VERSIONING_FILE);
        fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
    }

    fn write_versioning_config(
        &self,
        bucket: &str,
        config: &VersioningConfig,
    ) -> Result<(), StorageError> {
        let bucket_dir = self.root.join(bucket);
        if !bucket_dir.is_dir() {
            return Err(StorageError::BucketNotFound);
        }
        let data = serde_json::to_vec(config)
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        fs::write(bucket_dir.join(VERSIONING_FILE), data)?;
        Ok(())
    }

    fn read_ec_config(&self, bucket: &str) -> Option<EcConfig> {
        let path = self.root.join(bucket).join(EC_CONFIG_FILE);
        fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
    }

    fn write_ec_config(&self, bucket: &str, config: &EcConfig) -> Result<(), StorageError> {
        let bucket_dir = self.root.join(bucket);
        if !bucket_dir.is_dir() {
            return Err(StorageError::BucketNotFound);
        }
        let data = serde_json::to_vec(config)
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        fs::write(bucket_dir.join(EC_CONFIG_FILE), data)?;
        Ok(())
    }

    fn info(&self) -> BackendInfo {
        let (total_bytes, free_bytes) = disk_space(&self.root);
        let used_bytes = total_bytes.saturating_sub(free_bytes);
        BackendInfo {
            label: format!("local:{}", self.root.display()),
            backend_type: "local".to_string(),
            total_bytes: Some(total_bytes),
            used_bytes: Some(used_bytes),
            free_bytes: Some(free_bytes),
        }
    }
}

fn disk_space(path: &Path) -> (u64, u64) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free_available: u64 = 0;
        let mut total: u64 = 0;
        let mut _total_free: u64 = 0;
        unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_available as *mut u64 as *mut _,
                &mut total as *mut u64 as *mut _,
                &mut _total_free as *mut u64 as *mut _,
            );
        }
        (total, free_available)
    }

    #[cfg(not(target_os = "windows"))]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_path = match CString::new(path.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(_) => return (0, 0),
        };
        unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                let total = stat.f_blocks as u64 * stat.f_frsize as u64;
                let free = stat.f_bavail as u64 * stat.f_frsize as u64;
                (total, free)
            } else {
                (0, 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::metadata::ErasureMeta;
    use tempfile::TempDir;

    fn test_meta(index: usize) -> ObjectMeta {
        ObjectMeta {
            size: 100,
            etag: "abc".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                data: 2,
                parity: 2,
                index,
                distribution: vec![0, 1, 2, 3],
                epoch_id: 1,
                set_id: "set-a".to_string(),
                node_ids: vec![
                    "node-1".to_string(),
                    "node-2".to_string(),
                    "node-3".to_string(),
                    "node-4".to_string(),
                ],
                disk_ids: vec![
                    "disk-a".to_string(),
                    "disk-b".to_string(),
                    "disk-c".to_string(),
                    "disk-d".to_string(),
                ],
            },
            checksum: "deadbeef".to_string(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
        }
    }

    #[test]
    fn new_valid_dir() {
        let dir = TempDir::new().unwrap();
        assert!(LocalDisk::new(dir.path()).is_ok());
    }

    #[test]
    fn new_nonexistent_dir() {
        let path = PathBuf::from("/tmp/abixio_does_not_exist_12345");
        assert!(LocalDisk::new(&path).is_err());
    }

    #[test]
    fn make_bucket_and_exists() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        assert!(!disk.bucket_exists("test"));
        disk.make_bucket("test").unwrap();
        assert!(disk.bucket_exists("test"));
    }

    #[test]
    fn make_bucket_twice_errors() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        assert!(matches!(
            disk.make_bucket("test"),
            Err(StorageError::BucketExists)
        ));
    }

    #[test]
    fn bucket_exists_missing() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        assert!(!disk.bucket_exists("nope"));
    }

    #[test]
    fn list_buckets_ignores_tmp() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        disk.make_bucket("alpha").unwrap();
        disk.make_bucket("beta").unwrap();
        let buckets = disk.list_buckets().unwrap();
        assert_eq!(buckets, vec!["alpha", "beta"]);
        assert!(!buckets.contains(&TMP_DIR.to_string()));
    }

    #[test]
    fn write_read_shard_round_trip() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        let meta = test_meta(0);
        let data = b"hello world";
        disk.write_shard("test", "mykey", data, &meta).unwrap();
        let (read_data, read_meta) = disk.read_shard("test", "mykey").unwrap();
        assert_eq!(read_data, data);
        assert_eq!(read_meta, meta);
    }

    #[test]
    fn write_shard_nested_key() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "a/b/c", b"nested", &meta).unwrap();
        let (data, _) = disk.read_shard("test", "a/b/c").unwrap();
        assert_eq!(data, b"nested");
    }

    #[test]
    fn read_shard_missing_returns_error() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        assert!(matches!(
            disk.read_shard("test", "nope"),
            Err(StorageError::ObjectNotFound)
        ));
    }

    #[test]
    fn delete_object_then_read_fails() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "key", b"data", &meta).unwrap();
        disk.delete_object("test", "key").unwrap();
        assert!(matches!(
            disk.read_shard("test", "key"),
            Err(StorageError::ObjectNotFound)
        ));
    }

    #[test]
    fn list_objects_returns_written_keys() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "aaa", b"1", &meta).unwrap();
        disk.write_shard("test", "bbb", b"2", &meta).unwrap();
        let keys = disk.list_objects("test", "").unwrap();
        assert_eq!(keys, vec!["aaa", "bbb"]);
    }

    #[test]
    fn list_objects_with_prefix() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "logs/a", b"1", &meta).unwrap();
        disk.write_shard("test", "logs/b", b"2", &meta).unwrap();
        disk.write_shard("test", "data/c", b"3", &meta).unwrap();
        let keys = disk.list_objects("test", "logs/").unwrap();
        assert_eq!(keys, vec!["logs/a", "logs/b"]);
    }

    #[test]
    fn stat_object_returns_meta() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "key", b"data", &meta).unwrap();
        let loaded = disk.stat_object("test", "key").unwrap();
        assert_eq!(loaded, meta);
    }

    #[test]
    fn info_returns_local_backend() {
        let dir = TempDir::new().unwrap();
        let disk = LocalDisk::new(dir.path()).unwrap();
        let info = disk.info();
        assert_eq!(info.backend_type, "local");
        assert!(info.label.starts_with("local:"));
    }
}
