pub mod bitrot;
pub mod internode_auth;
pub mod local_volume;
pub mod remote_volume;
pub mod storage_server;
pub mod erasure_decode;
pub mod erasure_encode;
pub mod volume_pool;
pub mod metadata;
pub mod pathing;
pub mod volume;

use std::io;

use std::collections::HashMap;

use metadata::{
    BucketInfo, BucketSettings, ListOptions, ListResult, ObjectInfo, ObjectMeta, PutOptions,
    VersioningConfig,
};

/// Backend is the per-disk storage interface. Each erasure "disk" implements
/// this -- whether it is a local directory, a cloud drive, or anything else.
#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    async fn write_shard(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError>;

    async fn read_shard(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectMeta), StorageError>;

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError>;

    async fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, StorageError>;

    async fn list_buckets(&self) -> Result<Vec<String>, StorageError>;

    async fn make_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    fn bucket_exists(&self, bucket: &str) -> bool;

    fn bucket_created_at(&self, bucket: &str) -> u64;

    async fn stat_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StorageError>;

    async fn update_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), StorageError>;

    async fn read_meta_versions(&self, bucket: &str, key: &str) -> Result<Vec<ObjectMeta>, StorageError>;
    async fn write_meta_versions(
        &self,
        bucket: &str,
        key: &str,
        versions: &[ObjectMeta],
    ) -> Result<(), StorageError>;

    // versioned shard ops
    async fn write_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError>;

    async fn read_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectMeta), StorageError>;

    async fn delete_version_data(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError>;

    async fn read_bucket_settings(&self, bucket: &str) -> BucketSettings;
    async fn write_bucket_settings(
        &self,
        bucket: &str,
        settings: &BucketSettings,
    ) -> Result<(), StorageError>;

    fn info(&self) -> BackendInfo;

    /// Override volume_id (used by VolumePool during standalone init).
    fn set_volume_id(&mut self, _id: String) {}
}

/// Metadata about a storage backend, used for admin reporting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendInfo {
    pub label: String,
    pub volume_id: String,
    pub backend_type: String,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

/// Store is the primary storage interface. VolumePool implements this.
#[async_trait::async_trait]
pub trait Store: Send + Sync {
    async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        opts: PutOptions,
    ) -> Result<ObjectInfo, StorageError>;

    async fn get_object(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectInfo), StorageError>;

    async fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectInfo, StorageError>;

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError>;

    async fn make_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError>;

    async fn list_buckets(&self) -> Result<Vec<BucketInfo>, StorageError>;

    async fn list_objects(&self, bucket: &str, opts: ListOptions) -> Result<ListResult, StorageError>;

    async fn get_object_tags(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<HashMap<String, String>, StorageError>;

    async fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        tags: HashMap<String, String>,
    ) -> Result<(), StorageError>;

    async fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<(), StorageError>;

    // versioning
    async fn get_versioning_config(&self, bucket: &str)
    -> Result<Option<VersioningConfig>, StorageError>;
    async fn set_versioning_config(
        &self,
        bucket: &str,
        config: &VersioningConfig,
    ) -> Result<(), StorageError>;

    async fn put_object_versioned(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        opts: PutOptions,
        version_id: &str,
    ) -> Result<ObjectInfo, StorageError>;

    async fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectInfo), StorageError>;

    async fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError>;

    async fn add_delete_marker(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<ObjectInfo, StorageError>;

    async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, Vec<ObjectMeta>)>, StorageError>;

    // -- per-bucket FTT --

    async fn get_ftt(&self, bucket: &str) -> Result<Option<usize>, StorageError>;

    async fn set_ftt(&self, bucket: &str, ftt: usize) -> Result<(), StorageError>;

    // -- bucket settings --

    async fn get_bucket_settings(
        &self,
        bucket: &str,
    ) -> Result<BucketSettings, StorageError>;

    async fn set_bucket_settings(
        &self,
        bucket: &str,
        settings: &BucketSettings,
    ) -> Result<(), StorageError>;

    fn disk_count(&self) -> usize;
    async fn bucket_ec(&self, bucket: &str) -> (usize, usize);
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("bucket not found")]
    BucketNotFound,

    #[error("object not found")]
    ObjectNotFound,

    #[error("bucket already exists")]
    BucketExists,

    #[error("bucket not empty")]
    BucketNotEmpty,

    #[error("write quorum not met")]
    WriteQuorum,

    #[error("read quorum not met")]
    ReadQuorum,

    #[error("bitrot detected")]
    Bitrot,

    #[error("invalid config: {0}")]
    InvalidConfig(String),

    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid bucket name: {0}")]
    InvalidBucketName(String),

    #[error("invalid object key or prefix: {0}")]
    InvalidObjectKey(String),

    #[error("invalid version id: {0}")]
    InvalidVersionId(String),

    #[error("invalid upload id: {0}")]
    InvalidUploadId(String),

    #[error("internal error: {0}")]
    Internal(String),
}

/// Lock a Mutex, returning StorageError::Internal on poison.
pub fn lock_or_err<'a, T>(
    m: &'a std::sync::Mutex<T>,
    ctx: &str,
) -> Result<std::sync::MutexGuard<'a, T>, StorageError> {
    m.lock().map_err(|e| {
        tracing::error!("poisoned mutex ({}): {}", ctx, e);
        StorageError::Internal(format!("poisoned mutex: {}", ctx))
    })
}

/// Read-lock an RwLock, returning StorageError::Internal on poison.
pub fn read_or_err<'a, T>(
    rw: &'a std::sync::RwLock<T>,
    ctx: &str,
) -> Result<std::sync::RwLockReadGuard<'a, T>, StorageError> {
    rw.read().map_err(|e| {
        tracing::error!("poisoned rwlock read ({}): {}", ctx, e);
        StorageError::Internal(format!("poisoned rwlock: {}", ctx))
    })
}

/// Write-lock an RwLock, returning StorageError::Internal on poison.
pub fn write_or_err<'a, T>(
    rw: &'a std::sync::RwLock<T>,
    ctx: &str,
) -> Result<std::sync::RwLockWriteGuard<'a, T>, StorageError> {
    rw.write().map_err(|e| {
        tracing::error!("poisoned rwlock write ({}): {}", ctx, e);
        StorageError::Internal(format!("poisoned rwlock: {}", ctx))
    })
}
