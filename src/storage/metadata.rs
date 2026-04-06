use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Single meta.json per object, containing all version metadata.
/// Matches MinIO's xl.meta pattern.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObjectMetaFile {
    pub versions: Vec<ObjectMeta>,
}

/// Per-version metadata. Each entry in ObjectMetaFile.versions is one version.
/// For unversioned objects, there is exactly one entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObjectMeta {
    pub size: u64,
    pub etag: String,
    pub content_type: String,
    pub created_at: u64, // unix timestamp seconds
    pub erasure: ErasureMeta,
    pub checksum: String, // SHA256 hex of THIS shard (empty for multipart)
    #[serde(default)]
    pub user_metadata: HashMap<String, String>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
    #[serde(default)]
    pub version_id: String,
    #[serde(default)]
    pub is_latest: bool,
    #[serde(default)]
    pub is_delete_marker: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<PartEntry>,
}

/// Per-part metadata for multipart objects. Each part has its own
/// erasure coding and shard placement (like MinIO's xl.meta parts).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PartEntry {
    pub number: i32,
    pub size: u64,
    pub etag: String,
    pub erasure: ErasureMeta,
    pub checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErasureMeta {
    pub ftt: usize,
    pub index: usize, // which shard this disk holds
    #[serde(default)]
    pub epoch_id: u64,
    #[serde(default)]
    #[serde(alias = "set_id")]
    pub pool_id: String,
    #[serde(default)]
    pub volume_ids: Vec<String>,
}

impl ErasureMeta {
    /// Derive data shard count from volume_ids length and FTT.
    pub fn data(&self) -> usize {
        self.volume_ids.len() - self.ftt
    }

    /// Derive parity shard count (same as FTT).
    pub fn parity(&self) -> usize {
        self.ftt
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectInfo {
    pub bucket: String,
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub content_type: String,
    pub created_at: u64,
    pub user_metadata: HashMap<String, String>,
    pub tags: HashMap<String, String>,
    pub version_id: String,
    pub is_delete_marker: bool,
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
    pub user_metadata: HashMap<String, String>,
    pub tags: HashMap<String, String>,
    pub ec_ftt: Option<usize>,
}

// -- versioning --

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VersioningConfig {
    pub status: String, // "Enabled", "Suspended", or absent
}

// -- consolidated bucket settings --

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BucketSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versioning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ftt: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
}

impl ObjectMeta {
    /// True if this object was created via multipart upload.
    pub fn is_multipart(&self) -> bool {
        !self.parts.is_empty()
    }

    /// Two metas are quorum-compatible if all fields match except index and checksum
    /// (which are per-shard).
    pub fn quorum_eq(&self, other: &ObjectMeta) -> bool {
        self.size == other.size
            && self.etag == other.etag
            && self.content_type == other.content_type
            && self.created_at == other.created_at
            && self.erasure.ftt == other.erasure.ftt
            && self.erasure.epoch_id == other.erasure.epoch_id
            && self.erasure.pool_id == other.erasure.pool_id
            && self.erasure.volume_ids == other.erasure.volume_ids
            && self.user_metadata == other.user_metadata
            && self.tags == other.tags
    }
}

pub fn write_meta_file(path: &Path, meta: &ObjectMetaFile) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(meta)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(path, json)
}

pub fn read_meta_file(path: &Path) -> io::Result<ObjectMetaFile> {
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

    fn test_version(index: usize) -> ObjectMeta {
        ObjectMeta {
            size: 1024,
            etag: "abc".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                ftt: 2,
                index,
                epoch_id: 1,
                pool_id: "set-a".to_string(),
                volume_ids: vec![
                    "vol-a".to_string(),
                    "vol-b".to_string(),
                    "vol-c".to_string(),
                    "vol-d".to_string(),
                ],
            },
            checksum: "abc123".to_string(),
            user_metadata: HashMap::new(),
            tags: HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
        }
    }

    #[test]
    fn meta_file_serde_round_trip() {
        let mf = ObjectMetaFile {
            versions: vec![test_version(0)],
        };
        let json = serde_json::to_string(&mf).unwrap();
        let decoded: ObjectMetaFile = serde_json::from_str(&json).unwrap();
        assert_eq!(mf, decoded);
    }

    #[test]
    fn write_read_meta_file_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let mf = ObjectMetaFile {
            versions: vec![test_version(1)],
        };
        write_meta_file(&path, &mf).unwrap();
        let loaded = read_meta_file(&path).unwrap();
        assert_eq!(mf, loaded);
    }

    #[test]
    fn read_meta_file_missing_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert!(read_meta_file(&path).is_err());
    }

    #[test]
    fn quorum_eq_ignores_index_and_checksum() {
        let meta_a = test_version(0);
        let mut meta_b = test_version(3);
        meta_b.checksum = "different_checksum".to_string();
        assert!(meta_a.quorum_eq(&meta_b));
    }

    #[test]
    fn quorum_eq_different_size_not_compatible() {
        let meta_a = test_version(0);
        let mut meta_b = meta_a.clone();
        meta_b.size = 200;
        assert!(!meta_a.quorum_eq(&meta_b));
    }

    #[test]
    fn multi_version_meta_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let mf = ObjectMetaFile {
            versions: vec![
                ObjectMeta {
                    version_id: "uuid-2".to_string(),
                    is_latest: true,
                    ..test_version(0)
                },
                ObjectMeta {
                    version_id: "uuid-1".to_string(),
                    is_latest: false,
                    ..test_version(0)
                },
            ],
        };
        write_meta_file(&path, &mf).unwrap();
        let loaded = read_meta_file(&path).unwrap();
        assert_eq!(loaded.versions.len(), 2);
        assert_eq!(loaded.versions[0].version_id, "uuid-2");
        assert!(loaded.versions[0].is_latest);
    }
}
