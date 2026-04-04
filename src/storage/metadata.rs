use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObjectMeta {
    pub size: u64,
    pub etag: String,
    pub content_type: String,
    pub created_at: u64, // unix timestamp seconds
    pub erasure: ErasureMeta,
    pub checksum: String, // SHA256 hex of THIS shard
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErasureMeta {
    pub data: usize,
    pub parity: usize,
    pub index: usize,             // which shard this disk holds
    pub distribution: Vec<usize>, // permutation mapping shard -> disk
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectInfo {
    pub bucket: String,
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub content_type: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BucketInfo {
    pub name: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    pub prefix: String,
    pub delimiter: String,
    pub start_after: String,
    pub max_keys: usize,
    pub continuation_token: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListResult {
    pub objects: Vec<ObjectInfo>,
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: String,
}

#[derive(Debug, Clone, Default)]
pub struct PutOptions {
    pub content_type: String,
}

impl ObjectMeta {
    /// Two metas are quorum-compatible if all fields match except index and checksum
    /// (which are per-shard).
    pub fn quorum_eq(&self, other: &ObjectMeta) -> bool {
        self.size == other.size
            && self.etag == other.etag
            && self.content_type == other.content_type
            && self.created_at == other.created_at
            && self.erasure.data == other.erasure.data
            && self.erasure.parity == other.erasure.parity
            && self.erasure.distribution == other.erasure.distribution
    }
}

pub fn write_meta(path: &Path, meta: &ObjectMeta) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(meta)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(path, json)
}

pub fn read_meta(path: &Path) -> io::Result<ObjectMeta> {
    let data = fs::read(path)?;
    serde_json::from_slice(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn meta_serde_round_trip() {
        let meta = ObjectMeta {
            size: 1024,
            etag: "d41d8cd98f00b204e9800998ecf8427e".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                data: 2,
                parity: 2,
                index: 0,
                distribution: vec![2, 0, 3, 1],
            },
            checksum: "abc123".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let decoded: ObjectMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, decoded);
    }

    #[test]
    fn write_read_meta_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let meta = ObjectMeta {
            size: 512,
            etag: "abc".to_string(),
            content_type: "application/octet-stream".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                data: 2,
                parity: 2,
                index: 1,
                distribution: vec![0, 1, 2, 3],
            },
            checksum: "def456".to_string(),
        };
        write_meta(&path, &meta).unwrap();
        let loaded = read_meta(&path).unwrap();
        assert_eq!(meta, loaded);
    }

    #[test]
    fn read_meta_missing_file_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert!(read_meta(&path).is_err());
    }

    #[test]
    fn quorum_eq_ignores_index_and_checksum() {
        let meta_a = ObjectMeta {
            size: 100,
            etag: "aaa".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                data: 2,
                parity: 2,
                index: 0,
                distribution: vec![2, 0, 3, 1],
            },
            checksum: "checksum_shard_0".to_string(),
        };
        let meta_b = ObjectMeta {
            size: 100,
            etag: "aaa".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                data: 2,
                parity: 2,
                index: 3,
                distribution: vec![2, 0, 3, 1],
            },
            checksum: "checksum_shard_3".to_string(),
        };
        assert!(meta_a.quorum_eq(&meta_b));
    }

    #[test]
    fn quorum_eq_different_size_not_compatible() {
        let meta_a = ObjectMeta {
            size: 100,
            etag: "aaa".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                data: 2,
                parity: 2,
                index: 0,
                distribution: vec![0, 1, 2, 3],
            },
            checksum: "x".to_string(),
        };
        let meta_b = ObjectMeta {
            size: 200,
            ..meta_a.clone()
        };
        assert!(!meta_a.quorum_eq(&meta_b));
    }
}
