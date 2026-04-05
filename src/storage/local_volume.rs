use std::fs;
use std::path::{Path, PathBuf};

use super::metadata::{
    BucketSettings, ObjectMeta, ObjectMetaFile, read_meta_file, write_meta_file,
};
use super::pathing;
use super::{Backend, BackendInfo, StorageError};

pub struct LocalVolume {
    root: PathBuf,
    volume_id: String,
}

const TMP_DIR: &str = ".abixio.tmp";
const META_FILE: &str = "meta.json";
const BUCKET_SETTINGS_FILE: &str = "settings.json";

impl LocalVolume {
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
        // read volume_id from volume.json if available
        let volume_id = crate::storage::volume::read_volume_format(root)
            .map(|f| f.volume_id)
            .unwrap_or_default();
        Ok(Self {
            root: root.to_path_buf(),
            volume_id,
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
        pathing::validate_object_prefix(prefix)?;
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

impl LocalVolume {
    /// Write a part shard file (part.N) into the object directory.
    pub fn write_part_shard(
        &self,
        bucket: &str,
        key: &str,
        part_number: i32,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        fs::create_dir_all(&obj_dir)?;
        let part_path = obj_dir.join(format!("part.{}", part_number));
        fs::write(part_path, data)?;
        Ok(())
    }

    /// Read a part shard file (part.N) from the object directory.
    pub fn read_part_shard(
        &self,
        bucket: &str,
        key: &str,
        part_number: i32,
    ) -> Result<Vec<u8>, StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        let part_path = obj_dir.join(format!("part.{}", part_number));
        fs::read(&part_path).map_err(|_| StorageError::ObjectNotFound)
    }

    /// Write only meta.json without shard data (used by multipart complete).
    pub fn write_meta_only(
        &self,
        bucket: &str,
        key: &str,
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        fs::create_dir_all(&obj_dir)?;
        let mut version = meta.clone();
        version.is_latest = true;
        let mf = ObjectMetaFile {
            versions: vec![version],
        };
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf)
            .map_err(StorageError::Io)
    }
}

impl Backend for LocalVolume {
    fn write_shard(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        fs::create_dir_all(&obj_dir)?;

        // write shard data directly to key/shard.dat
        fs::write(pathing::object_shard_path(&self.root, bucket, key)?, data)?;

        // update meta.json: single version entry (unversioned = overwrite)
        let mut version = meta.clone();
        version.is_latest = true;
        let mf = ObjectMetaFile {
            versions: vec![version],
        };
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf)
            .map_err(StorageError::Io)?;

        Ok(())
    }

    fn read_shard(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?)
            .map_err(|_| StorageError::ObjectNotFound)?;

        // find latest non-delete-marker version
        let version = mf
            .versions
            .iter()
            .find(|v| !v.is_delete_marker)
            .ok_or(StorageError::ObjectNotFound)?;

        // determine shard path: unversioned = key/shard.dat, versioned = key/<uuid>/shard.dat
        let shard_path = if version.version_id.is_empty() {
            pathing::object_shard_path(&self.root, bucket, key)?
        } else {
            pathing::version_shard_path(&self.root, bucket, key, &version.version_id)?
        };

        let data = fs::read(&shard_path).map_err(|_| StorageError::ObjectNotFound)?;
        Ok((data, version.clone()))
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        fs::remove_dir_all(&obj_dir)?;
        Ok(())
    }

    fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, StorageError> {
        let bucket_dir = pathing::bucket_dir(&self.root, bucket)?;
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
        let path = pathing::bucket_dir(&self.root, bucket)?;
        if path.is_dir() {
            return Err(StorageError::BucketExists);
        }
        fs::create_dir_all(&path)?;
        Ok(())
    }

    fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let path = pathing::bucket_dir(&self.root, bucket)?;
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
        pathing::bucket_dir(&self.root, bucket)
            .map(|path| path.is_dir())
            .unwrap_or(false)
    }

    fn bucket_created_at(&self, bucket: &str) -> u64 {
        let Ok(path) = pathing::bucket_dir(&self.root, bucket) else {
            return 0;
        };
        fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn stat_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?)
            .map_err(|_| StorageError::ObjectNotFound)?;
        mf.versions
            .iter()
            .find(|v| !v.is_delete_marker)
            .cloned()
            .ok_or(StorageError::ObjectNotFound)
    }

    fn update_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        // replace the matching version entry (by index) in meta.json
        let mut mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?)
            .map_err(|_| StorageError::ObjectNotFound)?;
        if let Some(v) = mf
            .versions
            .iter_mut()
            .find(|v| v.erasure.index == meta.erasure.index || v.version_id == meta.version_id)
        {
            *v = meta.clone();
        }
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf)
            .map_err(StorageError::Io)
    }

    fn write_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let ver_dir = pathing::version_dir(&self.root, bucket, key, version_id)?;
        fs::create_dir_all(&ver_dir)?;

        // write shard data to key/<uuid>/shard.dat
        fs::write(pathing::version_shard_path(&self.root, bucket, key, version_id)?, data)?;

        // update meta.json: add new version entry at front, mark others as not latest
        let mut mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).unwrap_or(ObjectMetaFile {
            versions: Vec::new(),
        });
        for v in &mut mf.versions {
            v.is_latest = false;
        }
        let mut version = meta.clone();
        version.is_latest = true;
        version.version_id = version_id.to_string();
        mf.versions.insert(0, version);
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf)
            .map_err(StorageError::Io)?;

        Ok(())
    }

    fn read_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?)
            .map_err(|_| StorageError::ObjectNotFound)?;

        let version = mf
            .versions
            .iter()
            .find(|v| v.version_id == version_id)
            .ok_or(StorageError::ObjectNotFound)?;

        let shard_path = pathing::version_shard_path(&self.root, bucket, key, version_id)?;
        let data = fs::read(&shard_path).map_err(|_| StorageError::ObjectNotFound)?;
        Ok((data, version.clone()))
    }

    fn delete_version_data(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError> {
        // remove shard data dir
        let ver_dir = pathing::version_dir(&self.root, bucket, key, version_id)?;
        if ver_dir.is_dir() {
            fs::remove_dir_all(&ver_dir)?;
        }

        // remove version entry from meta.json
        if let Ok(mut mf) = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?) {
            mf.versions.retain(|v| v.version_id != version_id);
            // update is_latest
            if let Some(first) = mf.versions.first_mut() {
                first.is_latest = true;
            }
            let _ = write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf);
        }
        Ok(())
    }

    fn read_meta_versions(&self, bucket: &str, key: &str) -> Result<Vec<ObjectMeta>, StorageError> {
        match read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?) {
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
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        fs::create_dir_all(&obj_dir)?;
        let mf = ObjectMetaFile {
            versions: versions.to_vec(),
        };
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf)
            .map_err(StorageError::Io)
    }

    fn read_bucket_settings(&self, bucket: &str) -> BucketSettings {
        let Ok(path) = pathing::bucket_settings_path(&self.root, bucket) else {
            return BucketSettings::default();
        };
        fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    fn write_bucket_settings(
        &self,
        bucket: &str,
        settings: &BucketSettings,
    ) -> Result<(), StorageError> {
        let dir = pathing::bucket_settings_dir(&self.root, bucket)?;
        fs::create_dir_all(&dir)?;
        let data = serde_json::to_vec_pretty(settings)
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        fs::write(dir.join(BUCKET_SETTINGS_FILE), data)?;
        Ok(())
    }

    fn info(&self) -> BackendInfo {
        let (total_bytes, free_bytes) = disk_space(&self.root);
        let used_bytes = total_bytes.saturating_sub(free_bytes);
        BackendInfo {
            label: format!("local:{}", self.root.display()),
            volume_id: self.volume_id.clone(),
            backend_type: "local".to_string(),
            total_bytes: Some(total_bytes),
            used_bytes: Some(used_bytes),
            free_bytes: Some(free_bytes),
        }
    }

    fn set_volume_id(&mut self, id: String) {
        self.volume_id = id;
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
                ftt: 2,
                index,
                epoch_id: 1,
                pool_id: "set-a".to_string(),
                node_ids: vec![
                    "node-1".to_string(),
                    "node-2".to_string(),
                    "node-3".to_string(),
                    "node-4".to_string(),
                ],
                volume_ids: vec![
                    "vol-a".to_string(),
                    "vol-b".to_string(),
                    "vol-c".to_string(),
                    "vol-d".to_string(),
                ],
            },
            checksum: "deadbeef".to_string(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
        }
    }

    #[test]
    fn new_valid_dir() {
        let dir = TempDir::new().unwrap();
        assert!(LocalVolume::new(dir.path()).is_ok());
    }

    #[test]
    fn new_nonexistent_dir() {
        let path = PathBuf::from("/tmp/abixio_does_not_exist_12345");
        assert!(LocalVolume::new(&path).is_err());
    }

    #[test]
    fn make_bucket_and_exists() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        assert!(!disk.bucket_exists("test"));
        disk.make_bucket("test").unwrap();
        assert!(disk.bucket_exists("test"));
    }

    #[test]
    fn make_bucket_twice_errors() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        assert!(matches!(
            disk.make_bucket("test"),
            Err(StorageError::BucketExists)
        ));
    }

    #[test]
    fn bucket_exists_missing() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        assert!(!disk.bucket_exists("nope"));
    }

    #[test]
    fn list_buckets_ignores_tmp() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("alpha").unwrap();
        disk.make_bucket("beta").unwrap();
        let buckets = disk.list_buckets().unwrap();
        assert_eq!(buckets, vec!["alpha", "beta"]);
        assert!(!buckets.contains(&TMP_DIR.to_string()));
    }

    #[test]
    fn write_read_shard_round_trip() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
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
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "a/b/c", b"nested", &meta).unwrap();
        let (data, _) = disk.read_shard("test", "a/b/c").unwrap();
        assert_eq!(data, b"nested");
    }

    #[test]
    fn write_shard_rejects_hostile_key() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        let meta = test_meta(0);

        let err = disk.write_shard("test", "a/../b", b"bad", &meta).unwrap_err();
        assert!(matches!(err, StorageError::InvalidObjectKey(_)));
    }

    #[test]
    fn write_versioned_shard_rejects_hostile_version_id() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        let meta = test_meta(0);

        let err = disk
            .write_versioned_shard("test", "safe", "../escape", b"bad", &meta)
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidVersionId(_)));
    }

    #[test]
    fn read_shard_missing_returns_error() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        assert!(matches!(
            disk.read_shard("test", "nope"),
            Err(StorageError::ObjectNotFound)
        ));
    }

    #[test]
    fn delete_object_then_read_fails() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
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
        let disk = LocalVolume::new(dir.path()).unwrap();
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
        let disk = LocalVolume::new(dir.path()).unwrap();
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
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "key", b"data", &meta).unwrap();
        let loaded = disk.stat_object("test", "key").unwrap();
        assert_eq!(loaded, meta);
    }

    #[test]
    fn info_returns_local_backend() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        let info = disk.info();
        assert_eq!(info.backend_type, "local");
        assert!(info.label.starts_with("local:"));
    }
}
