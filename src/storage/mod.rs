pub mod bitrot;
pub mod disk;
pub mod erasure_decode;
pub mod erasure_encode;
pub mod erasure_set;
pub mod metadata;

use std::io;

use std::collections::HashMap;

use metadata::{
    BucketInfo, ListOptions, ListResult, ObjectInfo, ObjectMeta, PutOptions, VersionEntry,
    VersioningConfig,
};

/// Backend is the per-disk storage interface. Each erasure "disk" implements
/// this -- whether it is a local directory, a cloud drive, or anything else.
pub trait Backend: Send + Sync {
    fn write_shard(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError>;

    fn read_shard(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(Vec<u8>, ObjectMeta), StorageError>;

    fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError>;

    fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, StorageError>;

    fn list_buckets(&self) -> Result<Vec<String>, StorageError>;

    fn make_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    fn bucket_exists(&self, bucket: &str) -> bool;

    fn bucket_created_at(&self, bucket: &str) -> u64;

    fn stat_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StorageError>;

    fn update_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), StorageError>;

    // versioned shard ops
    fn write_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError>;

    fn read_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectMeta), StorageError>;

    fn delete_version_data(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError>;

    fn read_version_index(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Vec<VersionEntry>, StorageError>;

    fn write_version_index(
        &self,
        bucket: &str,
        key: &str,
        entries: &[VersionEntry],
    ) -> Result<(), StorageError>;

    fn read_versioning_config(&self, bucket: &str) -> Option<VersioningConfig>;
    fn write_versioning_config(
        &self,
        bucket: &str,
        config: &VersioningConfig,
    ) -> Result<(), StorageError>;

    fn info(&self) -> BackendInfo;
}

/// Metadata about a storage backend, used for admin reporting.
#[derive(Debug, Clone)]
pub struct BackendInfo {
    pub label: String,
    pub backend_type: String,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

/// Store is the primary storage interface. ErasureSet implements this.
pub trait Store: Send + Sync {
    fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        opts: PutOptions,
    ) -> Result<ObjectInfo, StorageError>;

    fn get_object(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectInfo), StorageError>;

    fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectInfo, StorageError>;

    fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError>;

    fn make_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError>;

    fn list_buckets(&self) -> Result<Vec<BucketInfo>, StorageError>;

    fn list_objects(&self, bucket: &str, opts: ListOptions) -> Result<ListResult, StorageError>;

    fn get_object_tags(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<HashMap<String, String>, StorageError>;

    fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        tags: HashMap<String, String>,
    ) -> Result<(), StorageError>;

    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<(), StorageError>;

    // versioning
    fn get_versioning_config(&self, bucket: &str) -> Result<Option<VersioningConfig>, StorageError>;
    fn set_versioning_config(
        &self,
        bucket: &str,
        config: &VersioningConfig,
    ) -> Result<(), StorageError>;

    fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectInfo), StorageError>;

    fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError>;

    fn list_object_versions(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, Vec<VersionEntry>)>, StorageError>;
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
}
