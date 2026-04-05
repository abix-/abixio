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
pub trait Backend: Send + Sync {
    fn write_shard(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError>;

    fn read_shard(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectMeta), StorageError>;

    fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError>;

    fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, StorageError>;

    fn list_buckets(&self) -> Result<Vec<String>, StorageError>;

    fn make_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    fn bucket_exists(&self, bucket: &str) -> bool;

    fn bucket_created_at(&self, bucket: &str) -> u64;

    fn stat_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StorageError>;

    fn update_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), StorageError>;

    fn read_meta_versions(&self, bucket: &str, key: &str) -> Result<Vec<ObjectMeta>, StorageError>;
    fn write_meta_versions(
        &self,
        bucket: &str,
        key: &str,
        versions: &[ObjectMeta],
    ) -> Result<(), StorageError>;

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

    fn read_bucket_settings(&self, bucket: &str) -> BucketSettings;
    fn write_bucket_settings(
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
    fn get_versioning_config(&self, bucket: &str)
    -> Result<Option<VersioningConfig>, StorageError>;
    fn set_versioning_config(
        &self,
        bucket: &str,
        config: &VersioningConfig,
    ) -> Result<(), StorageError>;

    fn put_object_versioned(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        opts: PutOptions,
        version_id: &str,
    ) -> Result<ObjectInfo, StorageError>;

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
    ) -> Result<Vec<(String, Vec<ObjectMeta>)>, StorageError>;

    // -- per-bucket FTT --

    fn get_ftt(&self, bucket: &str) -> Result<Option<usize>, StorageError>;

    fn set_ftt(&self, bucket: &str, ftt: usize) -> Result<(), StorageError>;

    // -- bucket settings --

    fn get_bucket_settings(
        &self,
        bucket: &str,
    ) -> Result<BucketSettings, StorageError>;

    fn set_bucket_settings(
        &self,
        bucket: &str,
        settings: &BucketSettings,
    ) -> Result<(), StorageError>;

    fn disk_count(&self) -> usize;
    fn bucket_ec(&self, bucket: &str) -> (usize, usize);
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
}
