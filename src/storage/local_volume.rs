use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

use super::metadata::{
    BucketSettings, ObjectMeta, ObjectMetaFile, read_meta_file, write_meta_file,
};
use super::pathing;
use super::{Backend, BackendInfo, ShardWriter, StorageError};

pub struct LocalVolume {
    root: PathBuf,
    volume_id: String,
}

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
        let tmp = root.join(".abixio.tmp");
        std::fs::create_dir_all(&tmp)?;
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

    async fn walk_keys(
        base: &Path,
        dir: &Path,
        prefix: &str,
        keys: &mut Vec<String>,
    ) -> Result<(), StorageError> {
        pathing::validate_object_prefix(prefix)?;
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let ft = entry.file_type().await?;
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
                        .map_err(|_| StorageError::Internal(
                            "object path not under base directory".into(),
                        ))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    if rel.starts_with(prefix) {
                        keys.push(rel);
                    }
                } else {
                    // recurse into subdirectory
                    Box::pin(Self::walk_keys(base, &entry.path(), prefix, keys)).await?;
                }
            }
        }
        Ok(())
    }
}

impl LocalVolume {
    /// Write a part shard file (part.N) into the object directory.
    pub async fn write_part_shard(
        &self,
        bucket: &str,
        key: &str,
        part_number: i32,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        tokio::fs::create_dir_all(&obj_dir).await?;
        let part_path = obj_dir.join(format!("part.{}", part_number));
        tokio::fs::write(part_path, data).await?;
        Ok(())
    }

    /// Read a part shard file (part.N) from the object directory.
    pub async fn read_part_shard(
        &self,
        bucket: &str,
        key: &str,
        part_number: i32,
    ) -> Result<Vec<u8>, StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        let part_path = obj_dir.join(format!("part.{}", part_number));
        tokio::fs::read(&part_path).await.map_err(|_| StorageError::ObjectNotFound)
    }

    /// Write only meta.json without shard data (used by multipart complete).
    pub async fn write_meta_only(
        &self,
        bucket: &str,
        key: &str,
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        tokio::fs::create_dir_all(&obj_dir).await?;
        let mut version = meta.clone();
        version.is_latest = true;
        let mf = ObjectMetaFile {
            versions: vec![version],
        };
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await
            .map_err(StorageError::Io)
    }
}

/// Streaming shard writer for local disk. Opens a file on creation,
/// appends chunks, writes meta on finalize.
pub struct LocalShardWriter {
    file: tokio::fs::File,
    root: PathBuf,
    bucket: String,
    key: String,
    version_id: Option<String>,
}

#[async_trait::async_trait]
impl ShardWriter for LocalShardWriter {
    async fn write_chunk(&mut self, data: &[u8]) -> Result<(), StorageError> {
        self.file.write_all(data).await?;
        Ok(())
    }

    async fn finalize(mut self: Box<Self>, meta: &ObjectMeta) -> Result<(), StorageError> {
        self.file.flush().await?;
        drop(self.file);

        if let Some(ref vid) = self.version_id {
            // versioned: update meta.json adding new version at front
            let meta_path = pathing::object_meta_path(&self.root, &self.bucket, &self.key)?;
            let mut mf = read_meta_file(&meta_path).await.unwrap_or(ObjectMetaFile {
                versions: Vec::new(),
            });
            for v in &mut mf.versions {
                v.is_latest = false;
            }
            let mut version = meta.clone();
            version.is_latest = true;
            version.version_id = vid.clone();
            mf.versions.insert(0, version);
            write_meta_file(&meta_path, &mf).await.map_err(StorageError::Io)?;
        } else {
            // unversioned: single version entry
            let mut version = meta.clone();
            version.is_latest = true;
            let mf = ObjectMetaFile {
                versions: vec![version],
            };
            let meta_path = pathing::object_meta_path(&self.root, &self.bucket, &self.key)?;
            write_meta_file(&meta_path, &mf).await.map_err(StorageError::Io)?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Backend for LocalVolume {
    async fn open_shard_writer(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<Box<dyn ShardWriter>, StorageError> {
        let shard_path = if let Some(vid) = version_id {
            let ver_dir = pathing::version_dir(&self.root, bucket, key, vid)?;
            tokio::fs::create_dir_all(&ver_dir).await?;
            pathing::version_shard_path(&self.root, bucket, key, vid)?
        } else {
            let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
            tokio::fs::create_dir_all(&obj_dir).await?;
            pathing::object_shard_path(&self.root, bucket, key)?
        };
        let file = tokio::fs::File::create(&shard_path).await?;
        Ok(Box::new(LocalShardWriter {
            file,
            root: self.root.clone(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            version_id: version_id.map(|s| s.to_string()),
        }))
    }

    async fn write_shard(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        tokio::fs::create_dir_all(&obj_dir).await?;

        // write shard data directly to key/shard.dat
        tokio::fs::write(pathing::object_shard_path(&self.root, bucket, key)?, data).await?;

        // update meta.json: single version entry (unversioned = overwrite)
        let mut version = meta.clone();
        version.is_latest = true;
        let mf = ObjectMetaFile {
            versions: vec![version],
        };
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await
            .map_err(StorageError::Io)?;

        Ok(())
    }

    async fn read_shard(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await
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

        let data = tokio::fs::read(&shard_path).await.map_err(|_| StorageError::ObjectNotFound)?;
        Ok((data, version.clone()))
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        tokio::fs::remove_dir_all(&obj_dir).await?;
        Ok(())
    }

    async fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, StorageError> {
        let bucket_dir = pathing::bucket_dir(&self.root, bucket)?;
        if !bucket_dir.is_dir() {
            return Err(StorageError::BucketNotFound);
        }
        let mut keys = Vec::new();
        Self::walk_keys(&bucket_dir, &bucket_dir, prefix, &mut keys).await?;
        keys.sort();
        Ok(keys)
    }

    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        let mut buckets = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if entry.file_type().await?.is_dir() {
                buckets.push(name);
            }
        }
        buckets.sort();
        Ok(buckets)
    }

    async fn make_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let path = pathing::bucket_dir(&self.root, bucket)?;
        if path.is_dir() {
            return Err(StorageError::BucketExists);
        }
        tokio::fs::create_dir_all(&path).await?;
        Ok(())
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let path = pathing::bucket_dir(&self.root, bucket)?;
        if !path.is_dir() {
            return Err(StorageError::BucketNotFound);
        }
        tokio::fs::remove_dir(&path).await.map_err(|e| {
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
        std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    async fn stat_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await
            .map_err(|_| StorageError::ObjectNotFound)?;
        mf.versions
            .iter()
            .find(|v| !v.is_delete_marker)
            .cloned()
            .ok_or(StorageError::ObjectNotFound)
    }

    async fn update_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        // replace the matching version entry (by index) in meta.json
        let mut mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await
            .map_err(|_| StorageError::ObjectNotFound)?;
        if let Some(v) = mf
            .versions
            .iter_mut()
            .find(|v| v.erasure.index == meta.erasure.index || v.version_id == meta.version_id)
        {
            *v = meta.clone();
        }
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await
            .map_err(StorageError::Io)
    }

    async fn write_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let ver_dir = pathing::version_dir(&self.root, bucket, key, version_id)?;
        tokio::fs::create_dir_all(&ver_dir).await?;

        // write shard data to key/<uuid>/shard.dat
        tokio::fs::write(pathing::version_shard_path(&self.root, bucket, key, version_id)?, data).await?;

        // update meta.json: add new version entry at front, mark others as not latest
        let mut mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await.unwrap_or(ObjectMetaFile {
            versions: Vec::new(),
        });
        for v in &mut mf.versions {
            v.is_latest = false;
        }
        let mut version = meta.clone();
        version.is_latest = true;
        version.version_id = version_id.to_string();
        mf.versions.insert(0, version);
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await
            .map_err(StorageError::Io)?;

        Ok(())
    }

    async fn read_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await
            .map_err(|_| StorageError::ObjectNotFound)?;

        let version = mf
            .versions
            .iter()
            .find(|v| v.version_id == version_id)
            .ok_or(StorageError::ObjectNotFound)?;

        let shard_path = pathing::version_shard_path(&self.root, bucket, key, version_id)?;
        let data = tokio::fs::read(&shard_path).await.map_err(|_| StorageError::ObjectNotFound)?;
        Ok((data, version.clone()))
    }

    async fn delete_version_data(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError> {
        // remove shard data dir
        let ver_dir = pathing::version_dir(&self.root, bucket, key, version_id)?;
        if ver_dir.is_dir() {
            tokio::fs::remove_dir_all(&ver_dir).await?;
        }

        // remove version entry from meta.json
        if let Ok(mut mf) = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await {
            mf.versions.retain(|v| v.version_id != version_id);
            // update is_latest
            if let Some(first) = mf.versions.first_mut() {
                first.is_latest = true;
            }
            let _ = write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await;
        }
        Ok(())
    }

    async fn read_meta_versions(&self, bucket: &str, key: &str) -> Result<Vec<ObjectMeta>, StorageError> {
        match read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await {
            Ok(mf) => Ok(mf.versions),
            Err(_) => Ok(Vec::new()),
        }
    }

    async fn write_meta_versions(
        &self,
        bucket: &str,
        key: &str,
        versions: &[ObjectMeta],
    ) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        tokio::fs::create_dir_all(&obj_dir).await?;
        let mf = ObjectMetaFile {
            versions: versions.to_vec(),
        };
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await
            .map_err(StorageError::Io)
    }

    async fn read_bucket_settings(&self, bucket: &str) -> BucketSettings {
        let Ok(path) = pathing::bucket_settings_path(&self.root, bucket) else {
            return BucketSettings::default();
        };
        tokio::fs::read(&path).await
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    async fn write_bucket_settings(
        &self,
        bucket: &str,
        settings: &BucketSettings,
    ) -> Result<(), StorageError> {
        let dir = pathing::bucket_settings_dir(&self.root, bucket)?;
        tokio::fs::create_dir_all(&dir).await?;
        let data = serde_json::to_vec_pretty(settings)
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        tokio::fs::write(dir.join(BUCKET_SETTINGS_FILE), data).await?;
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

    #[tokio::test]
    async fn new_valid_dir() {
        let dir = TempDir::new().unwrap();
        assert!(LocalVolume::new(dir.path()).is_ok());
    }

    #[tokio::test]
    async fn new_missing_dir() {
        let result = LocalVolume::new(Path::new("/nonexistent"));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn bucket_lifecycle() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        assert!(!disk.bucket_exists("test"));
        disk.make_bucket("test").await.unwrap();
        assert!(disk.bucket_exists("test"));
    }

    #[tokio::test]
    async fn make_bucket_already_exists() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        assert!(matches!(
            disk.make_bucket("test").await,
            Err(StorageError::BucketExists)
        ));
    }

    #[tokio::test]
    async fn bucket_exists_missing() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        assert!(!disk.bucket_exists("nope"));
    }

    #[tokio::test]
    async fn write_read_shard() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        let data = b"hello world";
        disk.write_shard("test", "mykey", data, &meta).await.unwrap();
        let (read_data, read_meta) = disk.read_shard("test", "mykey").await.unwrap();
        assert_eq!(read_data, data);
        assert_eq!(read_meta, meta);
    }

    #[tokio::test]
    async fn write_shard_nested_key() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "a/b/c", b"nested", &meta).await.unwrap();
        let (data, _) = disk.read_shard("test", "a/b/c").await.unwrap();
        assert_eq!(data, b"nested");
    }

    #[tokio::test]
    async fn write_shard_rejects_hostile_key() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);

        let err = disk.write_shard("test", "a/../b", b"bad", &meta).await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidObjectKey(_)));
    }

    #[tokio::test]
    async fn write_versioned_shard_rejects_hostile_version_id() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);

        let err = disk
            .write_versioned_shard("test", "safe", "../escape", b"bad", &meta)
            .await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidVersionId(_)));
    }

    #[tokio::test]
    async fn read_shard_missing_returns_error() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        assert!(matches!(
            disk.read_shard("test", "nope").await,
            Err(StorageError::ObjectNotFound)
        ));
    }

    #[tokio::test]
    async fn delete_object_removes_dir() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "key", b"data", &meta).await.unwrap();
        disk.delete_object("test", "key").await.unwrap();
        assert!(matches!(
            disk.read_shard("test", "key").await,
            Err(StorageError::ObjectNotFound)
        ));
    }

    #[tokio::test]
    async fn list_objects_returns_written_keys() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "aaa", b"1", &meta).await.unwrap();
        disk.write_shard("test", "bbb", b"2", &meta).await.unwrap();
        let keys = disk.list_objects("test", "").await.unwrap();
        assert_eq!(keys, vec!["aaa", "bbb"]);
    }

    #[tokio::test]
    async fn list_objects_with_prefix() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "logs/a", b"1", &meta).await.unwrap();
        disk.write_shard("test", "logs/b", b"2", &meta).await.unwrap();
        disk.write_shard("test", "data/c", b"3", &meta).await.unwrap();
        let keys = disk.list_objects("test", "logs/").await.unwrap();
        assert_eq!(keys, vec!["logs/a", "logs/b"]);
    }

    #[tokio::test]
    async fn stat_object_returns_meta() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "key", b"data", &meta).await.unwrap();
        let loaded = disk.stat_object("test", "key").await.unwrap();
        assert_eq!(loaded, meta);
    }

    #[tokio::test]
    async fn info_returns_local_backend() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        let info = disk.info();
        assert_eq!(info.backend_type, "local");
        assert!(info.label.starts_with("local:"));
    }
}
