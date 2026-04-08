//! Log-structured store: in-memory index + segment lifecycle.
//!
//! Manages multiple segments per disk volume. Routes small-object PUT/GET
//! through append-only segments with an in-memory needle index.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::needle::{self, Needle, NeedleLocation};
use super::segment::{ActiveSegment, SealedSegment, DEFAULT_SEGMENT_SIZE};
use super::metadata::ObjectMeta;
use super::StorageError;

/// In-memory index key: (bucket, key).
type IndexKey = (Arc<str>, Arc<str>);

/// The log-structured store for one disk volume.
pub struct LogStore {
    log_dir: PathBuf,
    index: HashMap<IndexKey, NeedleLocation>,
    active: Option<ActiveSegment>,
    sealed: Vec<SealedSegment>,
    next_segment_id: u32,
    segment_capacity: usize,
}

impl LogStore {
    /// Create or open a log store in the given directory.
    /// Scans existing segments and rebuilds the in-memory index.
    pub fn open(log_dir: &Path, segment_capacity: usize) -> Result<Self, StorageError> {
        std::fs::create_dir_all(log_dir)?;

        let mut store = Self {
            log_dir: log_dir.to_path_buf(),
            index: HashMap::new(),
            active: None,
            sealed: Vec::new(),
            next_segment_id: 1,
            segment_capacity,
        };

        // discover existing segment files
        let mut segment_paths: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(log_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "dat") {
                    segment_paths.push(path);
                }
            }
        }
        segment_paths.sort();

        // open all segments as sealed and rebuild index
        for path in &segment_paths {
            match SealedSegment::open(path) {
                Ok(seg) => {
                    let seg_id = seg.id();
                    if seg_id >= store.next_segment_id {
                        store.next_segment_id = seg_id + 1;
                    }
                    store.rebuild_index_from_segment(&seg);
                    store.sealed.push(seg);
                }
                Err(e) => {
                    tracing::warn!("skipping corrupt segment {:?}: {}", path, e);
                }
            }
        }

        // create a new active segment for writes
        store.rotate_active()?;

        Ok(store)
    }

    /// Rebuild index entries from a sealed segment.
    fn rebuild_index_from_segment(&mut self, seg: &SealedSegment) {
        let seg_id = seg.id();
        seg.scan(|flags, bucket, key, meta_offset, meta_len, data_offset, data_len, needle_offset| {
            let idx_key: IndexKey = (Arc::from(bucket), Arc::from(key));
            if flags == needle::FLAG_DELETE {
                self.index.remove(&idx_key);
            } else {
                // extract created_at from meta if possible
                let created_at = seg.slice(meta_offset, meta_len)
                    .and_then(|meta_bytes| Needle::decode_meta(meta_bytes).ok())
                    .map(|m| m.created_at as u32)
                    .unwrap_or(0);

                self.index.insert(idx_key, NeedleLocation {
                    segment_id: seg_id,
                    offset: needle_offset as u32,
                    meta_offset: meta_offset as u32,
                    meta_len: meta_len as u16,
                    data_offset: data_offset as u32,
                    data_len: data_len as u32,
                    created_at,
                });
            }
        }).ok();
    }

    /// Create a new active segment, sealing the current one if it exists.
    fn rotate_active(&mut self) -> Result<(), StorageError> {
        if let Some(active) = self.active.take() {
            let sealed = active.seal()?;
            self.sealed.push(sealed);
        }
        let seg_id = self.next_segment_id;
        self.next_segment_id += 1;
        self.active = Some(ActiveSegment::create(
            &self.log_dir,
            seg_id,
            self.segment_capacity,
        )?);
        Ok(())
    }

    /// Append a needle to the active segment. Rotates if full.
    /// Returns the needle location in the index.
    pub fn append(&mut self, needle: &Needle) -> Result<NeedleLocation, StorageError> {
        // try appending to active segment
        if let Some(ref mut active) = self.active {
            if let Some(mut loc) = active.append(needle)? {
                // extract created_at from meta
                if !needle.meta_bytes.is_empty() {
                    if let Ok(meta) = Needle::decode_meta(&needle.meta_bytes) {
                        loc.created_at = meta.created_at as u32;
                    }
                }
                let idx_key: IndexKey = (Arc::from(needle.bucket.as_str()), Arc::from(needle.key.as_str()));
                self.index.insert(idx_key, loc);
                return Ok(loc);
            }
        }

        // active segment full -- rotate and retry
        self.rotate_active()?;

        if let Some(ref mut active) = self.active {
            if let Some(mut loc) = active.append(needle)? {
                if !needle.meta_bytes.is_empty() {
                    if let Ok(meta) = Needle::decode_meta(&needle.meta_bytes) {
                        loc.created_at = meta.created_at as u32;
                    }
                }
                let idx_key: IndexKey = (Arc::from(needle.bucket.as_str()), Arc::from(needle.key.as_str()));
                self.index.insert(idx_key, loc);
                return Ok(loc);
            }
        }

        Err(StorageError::Internal("needle too large for segment".into()))
    }

    /// Append a tombstone for a key.
    pub fn delete(&mut self, bucket: &str, key: &str) -> Result<(), StorageError> {
        let tomb = Needle::tombstone(bucket, key);
        // append tombstone to log (for crash recovery)
        if let Some(ref mut active) = self.active {
            if active.append(&tomb)?.is_none() {
                self.rotate_active()?;
                if let Some(ref mut active) = self.active {
                    active.append(&tomb)?;
                }
            }
        }
        let idx_key: IndexKey = (Arc::from(bucket), Arc::from(key));
        self.index.remove(&idx_key);
        Ok(())
    }

    /// Fsync the active segment.
    pub fn fsync(&self) -> Result<(), StorageError> {
        if let Some(ref active) = self.active {
            active.fsync()?;
        }
        Ok(())
    }

    /// Look up a needle location by bucket + key.
    pub fn get(&self, bucket: &str, key: &str) -> Option<&NeedleLocation> {
        let idx_key: IndexKey = (Arc::from(bucket), Arc::from(key));
        self.index.get(&idx_key)
    }

    /// Check if a key exists in the log index.
    pub fn contains(&self, bucket: &str, key: &str) -> bool {
        let idx_key: IndexKey = (Arc::from(bucket), Arc::from(key));
        self.index.contains_key(&idx_key)
    }

    /// Read shard data for a needle. Sealed segments use mmap (zero-copy),
    /// active segment uses pread (copies data, no seal needed).
    pub fn read_data_vec(&self, loc: &NeedleLocation) -> Option<Vec<u8>> {
        // check sealed segments (mmap, zero-copy)
        if let Some(seg) = self.sealed.iter().find(|s| s.id() == loc.segment_id) {
            return seg.slice(loc.data_offset as usize, loc.data_len as usize)
                .map(|s| s.to_vec());
        }
        // check active segment (pread, no seal)
        if let Some(ref active) = self.active {
            if active.id() == loc.segment_id {
                return active.read_at(loc.data_offset as usize, loc.data_len as usize).ok();
            }
        }
        None
    }

    /// Read metadata bytes for a needle.
    pub fn read_meta_vec(&self, loc: &NeedleLocation) -> Option<Vec<u8>> {
        if let Some(seg) = self.sealed.iter().find(|s| s.id() == loc.segment_id) {
            return seg.slice(loc.meta_offset as usize, loc.meta_len as usize)
                .map(|s| s.to_vec());
        }
        if let Some(ref active) = self.active {
            if active.id() == loc.segment_id {
                return active.read_at(loc.meta_offset as usize, loc.meta_len as usize).ok();
            }
        }
        None
    }

    /// Read and decode ObjectMeta for a needle.
    pub fn read_meta(&self, loc: &NeedleLocation) -> Result<ObjectMeta, StorageError> {
        let meta_bytes = self.read_meta_vec(loc)
            .ok_or(StorageError::ObjectNotFound)?;
        Needle::decode_meta(&meta_bytes)
    }

    /// List all keys in the index for a given bucket (for LIST operations).
    pub fn list_keys(&self, bucket: &str, prefix: &str) -> Vec<String> {
        self.index
            .keys()
            .filter(|(b, k)| b.as_ref() == bucket && k.starts_with(prefix))
            .map(|(_, k)| k.to_string())
            .collect()
    }

    /// Number of objects in the index.
    pub fn object_count(&self) -> usize {
        self.index.len()
    }

    /// Number of segments (sealed + active).
    pub fn segment_count(&self) -> usize {
        self.sealed.len() + if self.active.is_some() { 1 } else { 0 }
    }

    /// Seal the active segment and mmap it for reads.
    /// Used when you need to read from recently-written data.
    pub fn seal_active_for_reads(&mut self) -> Result<(), StorageError> {
        if let Some(active) = self.active.take() {
            if active.data_size() > super::segment::SUPERBLOCK_SIZE {
                let sealed = active.seal()?;
                self.sealed.push(sealed);
            }
        }
        // create new active
        let seg_id = self.next_segment_id;
        self.next_segment_id += 1;
        self.active = Some(ActiveSegment::create(
            &self.log_dir,
            seg_id,
            self.segment_capacity,
        )?);
        Ok(())
    }
}

// SAFETY: LogStore uses no interior mutability that would be unsound across threads.
// Access is serialized by the caller (VolumePool holds &mut or Mutex).
unsafe impl Send for LogStore {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::metadata::ErasureMeta;
    use tempfile::TempDir;

    fn test_meta() -> ObjectMeta {
        ObjectMeta {
            size: 4096,
            etag: "abc".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                ftt: 0,
                index: 0,
                epoch_id: 1,
                volume_ids: vec!["v1".to_string()],
            },
            checksum: "dead".to_string(),
            user_metadata: HashMap::new(),
            tags: HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
                inline_data: None,
        }
    }

    #[test]
    fn put_and_get() {
        let tmp = TempDir::new().unwrap();
        let mut store = LogStore::open(tmp.path(), 1024 * 1024).unwrap();

        let needle = Needle::new("mybucket", "hello.txt", &test_meta(), b"hello world").unwrap();
        let loc = store.append(&needle).unwrap();
        store.fsync().unwrap();

        // reads work from active segment (no seal needed)
        assert!(store.contains("mybucket", "hello.txt"));
        let got_loc = store.get("mybucket", "hello.txt").unwrap();
        assert_eq!(got_loc.segment_id, loc.segment_id);

        let data = store.read_data_vec(got_loc).unwrap();
        assert_eq!(data, b"hello world");

        let meta = store.read_meta(got_loc).unwrap();
        assert_eq!(meta.size, 4096);
        assert_eq!(meta.etag, "abc");
    }

    #[test]
    fn delete_removes_from_index() {
        let tmp = TempDir::new().unwrap();
        let mut store = LogStore::open(tmp.path(), 1024 * 1024).unwrap();

        let needle = Needle::new("b", "k", &test_meta(), b"data").unwrap();
        store.append(&needle).unwrap();
        assert!(store.contains("b", "k"));

        store.delete("b", "k").unwrap();
        assert!(!store.contains("b", "k"));
    }

    #[test]
    fn overwrite_updates_index() {
        let tmp = TempDir::new().unwrap();
        let mut store = LogStore::open(tmp.path(), 1024 * 1024).unwrap();

        let n1 = Needle::new("b", "k", &test_meta(), b"version1").unwrap();
        store.append(&n1).unwrap();

        let n2 = Needle::new("b", "k", &test_meta(), b"version2").unwrap();
        store.append(&n2).unwrap();

        let loc = store.get("b", "k").unwrap();
        let data = store.read_data_vec(loc).unwrap();
        assert_eq!(data, b"version2");
    }

    #[test]
    fn segment_rotation_on_full() {
        let tmp = TempDir::new().unwrap();
        // tiny segments: force rotation
        let mut store = LogStore::open(tmp.path(), 512).unwrap();

        let data = vec![0x42u8; 100];
        for i in 0..10 {
            let needle = Needle::new("b", &format!("k{}", i), &test_meta(), &data).unwrap();
            store.append(&needle).unwrap();
        }

        // should have rotated multiple times
        assert!(store.segment_count() > 1);
        assert_eq!(store.object_count(), 10);
    }

    #[test]
    fn crash_recovery_rebuilds_index() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();

        // write some objects
        {
            let mut store = LogStore::open(&dir, 1024 * 1024).unwrap();
            for i in 0..5 {
                let needle = Needle::new("b", &format!("k{}", i), &test_meta(), b"data").unwrap();
                store.append(&needle).unwrap();
            }
            store.delete("b", "k2").unwrap();
            store.fsync().unwrap();
        }
        // store dropped -- simulate crash

        // reopen -- should rebuild index from segments
        let store = LogStore::open(&dir, 1024 * 1024).unwrap();
        assert_eq!(store.object_count(), 4); // 5 written - 1 deleted
        assert!(store.contains("b", "k0"));
        assert!(store.contains("b", "k1"));
        assert!(!store.contains("b", "k2")); // deleted
        assert!(store.contains("b", "k3"));
        assert!(store.contains("b", "k4"));
    }

    #[test]
    fn list_keys_with_prefix() {
        let tmp = TempDir::new().unwrap();
        let mut store = LogStore::open(tmp.path(), 1024 * 1024).unwrap();

        let n1 = Needle::new("b", "photos/a.jpg", &test_meta(), b"a").unwrap();
        let n2 = Needle::new("b", "photos/b.jpg", &test_meta(), b"b").unwrap();
        let n3 = Needle::new("b", "docs/readme.md", &test_meta(), b"c").unwrap();
        store.append(&n1).unwrap();
        store.append(&n2).unwrap();
        store.append(&n3).unwrap();

        let mut keys = store.list_keys("b", "photos/");
        keys.sort();
        assert_eq!(keys, vec!["photos/a.jpg", "photos/b.jpg"]);

        let all = store.list_keys("b", "");
        assert_eq!(all.len(), 3);
    }
}
