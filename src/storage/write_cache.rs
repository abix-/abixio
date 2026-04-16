//! RAM write cache with peer replication for low-latency PUT.
//!
//! Writes go to a DashMap in RAM, ack immediately. Background flush
//! destages to disk asynchronously. No disk I/O on the write hot path.
//! Design derived from our own latency measurements -- the disk write
//! is microseconds, but the filesystem call path is hundreds of
//! microseconds. See docs/write-cache.md for the request trace.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use super::metadata::ObjectMeta;

/// Key type for the cache: (bucket, key).
type CacheKey = (Arc<str>, Arc<str>);

/// A cached object ready to be flushed to disk.
/// Shards stored as Bytes (ref-counted) for zero-copy GET responses.
///
/// `primary_node_id` identifies the node that owns this entry. Only the
/// primary flushes to disk -- peers hold replicas in RAM for crash
/// fallback but never flush, because their `distribution` indexes point
/// into the primary's `VolumePool`, not their own.
#[derive(Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// RS-encoded shard data (one per disk). Bytes for zero-copy cloning.
    pub shards: Vec<bytes::Bytes>,
    /// Per-shard metadata.
    pub metas: Vec<ObjectMeta>,
    /// Which disk index gets which shard.
    pub distribution: Vec<usize>,
    /// Original object size.
    pub original_size: u64,
    pub content_type: String,
    pub etag: String,
    pub created_at: u64,
    pub user_metadata: HashMap<String, String>,
    pub tags: HashMap<String, String>,
    /// Unix-nanos timestamp when this entry was inserted. Wire-portable
    /// because `Instant` is not serializable; age comparisons on the
    /// primary still use `Instant` via `cached_at_local`.
    pub cached_at_unix_nanos: u64,
    /// Node-local Instant used for age comparisons. Skipped on the wire
    /// (peers recompute on receive).
    #[serde(skip, default = "Instant::now")]
    pub cached_at_local: Instant,
    /// Node id that owns and will flush this entry. Peers use this to
    /// decide not to flush.
    #[serde(default)]
    pub primary_node_id: String,
}

/// Current unix-nanos helper used when stamping new cache entries.
pub fn unix_nanos_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

impl CacheEntry {
    /// Approximate memory footprint of this entry.
    fn mem_size(&self) -> u64 {
        let shard_bytes: usize = self.shards.iter().map(|s| s.len()).sum();
        // shards + metadata overhead (~200 bytes per meta) + strings
        (shard_bytes
            + self.metas.len() * 200
            + self.etag.len()
            + self.content_type.len()
            + self.primary_node_id.len()
            + 128) as u64
    }
}

/// Lock-free RAM write cache.
pub struct WriteCache {
    entries: DashMap<CacheKey, CacheEntry>,
    size_bytes: AtomicU64,
    max_bytes: u64,
}

impl WriteCache {
    /// Create a new write cache with the given max size in bytes.
    pub fn new(max_bytes: u64) -> Self {
        Self {
            entries: DashMap::new(),
            size_bytes: AtomicU64::new(0),
            max_bytes,
        }
    }

    /// Insert a cached object. Returns true if inserted, false if cache full.
    pub fn insert(
        &self,
        bucket: &str,
        key: &str,
        entry: CacheEntry,
    ) -> bool {
        let entry_size = entry.mem_size();
        let current = self.size_bytes.load(Ordering::Relaxed);
        if current + entry_size > self.max_bytes {
            return false; // cache full, caller should write to disk
        }

        let cache_key: CacheKey = (Arc::from(bucket), Arc::from(key));

        // if overwriting, subtract old entry size
        if let Some(old) = self.entries.get(&cache_key) {
            self.size_bytes.fetch_sub(old.mem_size(), Ordering::Relaxed);
        }

        self.size_bytes.fetch_add(entry_size, Ordering::Relaxed);
        self.entries.insert(cache_key, entry);
        true
    }

    /// Get a reference to a cached entry.
    pub fn get(&self, bucket: &str, key: &str) -> Option<dashmap::mapref::one::Ref<CacheKey, CacheEntry>> {
        let cache_key: CacheKey = (Arc::from(bucket), Arc::from(key));
        self.entries.get(&cache_key)
    }

    /// Remove a cached entry.
    pub fn remove(&self, bucket: &str, key: &str) -> Option<CacheEntry> {
        let cache_key: CacheKey = (Arc::from(bucket), Arc::from(key));
        self.entries.remove(&cache_key).map(|(_, v)| {
            self.size_bytes.fetch_sub(v.mem_size(), Ordering::Relaxed);
            v
        })
    }

    /// Check if key exists in cache.
    pub fn contains(&self, bucket: &str, key: &str) -> bool {
        let cache_key: CacheKey = (Arc::from(bucket), Arc::from(key));
        self.entries.contains_key(&cache_key)
    }

    /// List keys in cache for a given bucket + prefix.
    pub fn list_keys(&self, bucket: &str, prefix: &str) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.key().0.as_ref() == bucket && e.key().1.starts_with(prefix))
            .map(|e| e.key().1.to_string())
            .collect()
    }

    /// Drain entries older than the given duration. Returns (bucket, key, entry) tuples.
    /// Only drains entries whose `primary_node_id` is empty (single-node mode)
    /// or matches `local_node_id`. Replicas (non-matching primary) are kept
    /// because their `distribution` indexes point into the primary's
    /// VolumePool, not ours.
    pub fn drain_older_than(&self, age: Duration) -> Vec<(String, String, CacheEntry)> {
        self.drain_primary_older_than("", age)
    }

    /// Like `drain_older_than` but filters by owner. Entries with an empty
    /// primary_node_id are treated as primary-owned by the caller (legacy /
    /// single-node mode).
    pub fn drain_primary_older_than(
        &self,
        local_node_id: &str,
        age: Duration,
    ) -> Vec<(String, String, CacheEntry)> {
        let now = Instant::now();
        let mut drained = Vec::new();

        let old_keys: Vec<CacheKey> = self.entries
            .iter()
            .filter(|e| now.duration_since(e.cached_at_local) >= age)
            .filter(|e| e.primary_node_id.is_empty() || e.primary_node_id == local_node_id)
            .map(|e| e.key().clone())
            .collect();

        for key in old_keys {
            if let Some((k, entry)) = self.entries.remove(&key) {
                self.size_bytes.fetch_sub(entry.mem_size(), Ordering::Relaxed);
                drained.push((k.0.to_string(), k.1.to_string(), entry));
            }
        }

        drained
    }

    /// Drain all entries whose primary_node_id matches the given id,
    /// regardless of age. Used by phase-8 restart recovery: peer sends
    /// back entries where the caller is primary.
    pub fn drain_by_primary(&self, primary_node_id: &str) -> Vec<(String, String, CacheEntry)> {
        let mut drained = Vec::new();
        let keys: Vec<CacheKey> = self.entries
            .iter()
            .filter(|e| e.primary_node_id == primary_node_id)
            .map(|e| e.key().clone())
            .collect();
        for key in keys {
            if let Some((k, entry)) = self.entries.remove(&key) {
                self.size_bytes.fetch_sub(entry.mem_size(), Ordering::Relaxed);
                drained.push((k.0.to_string(), k.1.to_string(), entry));
            }
        }
        drained
    }

    /// Current cache size in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes.load(Ordering::Relaxed)
    }

    /// Max cache size in bytes.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Number of entries in cache.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache has space for an entry of the given size.
    pub fn has_space(&self, entry_size: u64) -> bool {
        self.size_bytes.load(Ordering::Relaxed) + entry_size <= self.max_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::metadata::{ErasureMeta, ObjectMeta};

    fn test_meta() -> ObjectMeta {
        ObjectMeta {
            size: 4096,
            etag: "abc".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta { ftt: 0, index: 0, epoch_id: 1, volume_ids: vec!["v1".to_string()] },
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

    fn test_entry() -> CacheEntry {
        CacheEntry {
            shards: vec![bytes::Bytes::from(vec![0x42u8; 1365])],
            metas: vec![test_meta()],
            distribution: vec![0],
            original_size: 4096,
            content_type: "text/plain".to_string(),
            etag: "abc".to_string(),
            created_at: 1700000000,
            user_metadata: HashMap::new(),
            tags: HashMap::new(),
            cached_at_unix_nanos: unix_nanos_now(),
            cached_at_local: Instant::now(),
            primary_node_id: String::new(),
        }
    }

    #[test]
    fn insert_and_get() {
        let cache = WriteCache::new(1024 * 1024);
        assert!(cache.insert("bucket", "key1", test_entry()));
        assert!(cache.contains("bucket", "key1"));
        assert!(!cache.contains("bucket", "key2"));

        let entry = cache.get("bucket", "key1").unwrap();
        assert_eq!(entry.original_size, 4096);
        assert_eq!(entry.shards[0].len(), 1365);
    }

    #[test]
    fn remove() {
        let cache = WriteCache::new(1024 * 1024);
        cache.insert("b", "k", test_entry());
        assert_eq!(cache.entry_count(), 1);

        let removed = cache.remove("b", "k").unwrap();
        assert_eq!(removed.original_size, 4096);
        assert_eq!(cache.entry_count(), 0);
        assert!(!cache.contains("b", "k"));
    }

    #[test]
    fn cache_full_returns_false() {
        let cache = WriteCache::new(100); // tiny cache
        assert!(!cache.insert("b", "k", test_entry())); // too big
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn overwrite_updates_size() {
        let cache = WriteCache::new(1024 * 1024);
        cache.insert("b", "k", test_entry());
        let size1 = cache.size_bytes();

        cache.insert("b", "k", test_entry()); // overwrite
        let size2 = cache.size_bytes();
        assert_eq!(size1, size2); // same size entry
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn drain_older_than() {
        let cache = WriteCache::new(1024 * 1024);
        cache.insert("b", "k1", test_entry());
        cache.insert("b", "k2", test_entry());

        // nothing old enough yet
        let drained = cache.drain_older_than(Duration::from_secs(60));
        assert!(drained.is_empty());

        // drain everything (age=0)
        let drained = cache.drain_older_than(Duration::from_nanos(0));
        assert_eq!(drained.len(), 2);
        assert_eq!(cache.entry_count(), 0);
        assert_eq!(cache.size_bytes(), 0);
    }

    #[test]
    fn drain_respects_primary_node_id() {
        let cache = WriteCache::new(1024 * 1024);
        let mut a = test_entry();
        a.primary_node_id = "node-a".to_string();
        let mut b = test_entry();
        b.primary_node_id = "node-b".to_string();
        cache.insert("b", "owned", a);
        cache.insert("b", "replica", b);

        let drained = cache.drain_primary_older_than("node-a", Duration::from_nanos(0));
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].1, "owned");
        // replica still in cache
        assert!(cache.contains("b", "replica"));
        assert!(!cache.contains("b", "owned"));
    }

    #[test]
    fn drain_by_primary_pulls_replicas_only() {
        let cache = WriteCache::new(1024 * 1024);
        let mut a = test_entry();
        a.primary_node_id = "node-a".to_string();
        let mut b = test_entry();
        b.primary_node_id = "node-b".to_string();
        cache.insert("b", "k1", a);
        cache.insert("b", "k2", b);

        let pulled = cache.drain_by_primary("node-a");
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].1, "k1");
        assert!(!cache.contains("b", "k1"));
        assert!(cache.contains("b", "k2"));
    }

    #[test]
    fn cache_entry_serde_roundtrip() {
        let mut entry = test_entry();
        entry.primary_node_id = "node-x".to_string();
        let bytes = rmp_serde::to_vec(&entry).expect("serialize");
        let decoded: CacheEntry = rmp_serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded.original_size, entry.original_size);
        assert_eq!(decoded.etag, entry.etag);
        assert_eq!(decoded.primary_node_id, "node-x");
        assert_eq!(decoded.shards.len(), entry.shards.len());
        assert_eq!(decoded.shards[0], entry.shards[0]);
        assert_eq!(decoded.cached_at_unix_nanos, entry.cached_at_unix_nanos);
    }

    #[test]
    fn list_keys_with_prefix() {
        let cache = WriteCache::new(1024 * 1024);
        cache.insert("b", "photos/a.jpg", test_entry());
        cache.insert("b", "photos/b.jpg", test_entry());
        cache.insert("b", "docs/readme", test_entry());

        let mut keys = cache.list_keys("b", "photos/");
        keys.sort();
        assert_eq!(keys, vec!["photos/a.jpg", "photos/b.jpg"]);
    }
}
