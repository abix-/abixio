pub mod bitrot;
pub mod disk;
pub mod erasure_decode;
pub mod erasure_encode;
pub mod erasure_set;
pub mod metadata;

use std::io;

use metadata::{BucketInfo, ListOptions, ListResult, ObjectInfo, PutOptions};

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

    fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError>;

    fn list_buckets(&self) -> Result<Vec<BucketInfo>, StorageError>;

    fn list_objects(&self, bucket: &str, opts: ListOptions) -> Result<ListResult, StorageError>;
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("bucket not found")]
    BucketNotFound,

    #[error("object not found")]
    ObjectNotFound,

    #[error("bucket already exists")]
    BucketExists,

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
