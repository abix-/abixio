use std::fs;
use std::path::{Path, PathBuf};

use super::{Backend, BackendInfo, StorageError};
use super::metadata::{ObjectMeta, read_meta, write_meta};

pub struct LocalDisk {
    root: PathBuf,
}

const TMP_DIR: &str = ".abixio.tmp";
const SHARD_FILE: &str = "shard.dat";
const META_FILE: &str = "meta.json";

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
                // check if this dir is an object (has shard.dat)
                let shard_path = entry.path().join(SHARD_FILE);
                if shard_path.exists() {
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
        let tmp_id = uuid::Uuid::new_v4().to_string();
        let tmp_dir = self.root.join(TMP_DIR).join(&tmp_id);

        fs::create_dir_all(&tmp_dir)?;

        // write shard data to tmp
        fs::write(tmp_dir.join(SHARD_FILE), data)?;

        // write metadata to tmp
        write_meta(&tmp_dir.join(META_FILE), meta).map_err(StorageError::Io)?;

        // ensure parent of final destination exists
        if let Some(parent) = obj_dir.parent() {
            fs::create_dir_all(parent)?;
        }

        // atomic rename (same filesystem)
        // on Windows, target must not exist for rename to succeed
        if obj_dir.exists() {
            fs::remove_dir_all(&obj_dir)?;
        }
        fs::rename(&tmp_dir, &obj_dir)?;

        Ok(())
    }

    fn read_shard(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let data = fs::read(obj_dir.join(SHARD_FILE)).map_err(|_| StorageError::ObjectNotFound)?;
        let meta = read_meta(&obj_dir.join(META_FILE)).map_err(|_| StorageError::ObjectNotFound)?;
        Ok((data, meta))
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
        read_meta(&obj_dir.join(META_FILE)).map_err(|_| StorageError::ObjectNotFound)
    }

    fn update_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), StorageError> {
        let obj_dir = self.root.join(bucket).join(key);
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        write_meta(&obj_dir.join(META_FILE), meta).map_err(StorageError::Io)
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
            },
            checksum: "deadbeef".to_string(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
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
