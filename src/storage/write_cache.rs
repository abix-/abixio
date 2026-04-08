//! RAM write cache (PernixData FVP style).
//!
//! Writes go to a DashMap in RAM, ack immediately. Background flush
//! destages to disk asynchronously. No disk I/O on the write hot path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use super::metadata::ObjectMeta;

/// Key type for the cache: (bucket, key).
type CacheKey = (Arc<str>, Arc<str>);

/// A cached object ready to be flushed to disk.
/// Shards stored as Bytes (ref-counted) for zero-copy GET responses.
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
    /// When this entry was inserted into cache.
    pub cached_at: Instant,
}

impl CacheEntry {
    /// Approximate memory footprint of this entry.
    fn mem_size(&self) -> u64 {
        let shard_bytes: usize = self.shards.iter().map(|s| s.len()).sum();
        // shards + metadata overhead (~200 bytes per meta) + strings
        (shard_bytes + self.metas.len() * 200 + self.etag.len() + self.content_type.len() + 128) as u64
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
    pub fn drain_older_than(&self, age: Duration) -> Vec<(String, String, CacheEntry)> {
        let now = Instant::now();
        let mut drained = Vec::new();

        // collect keys to remove (can't remove while iterating DashMap)
        let old_keys: Vec<CacheKey> = self.entries
            .iter()
            .filter(|e| now.duration_since(e.cached_at) >= age)
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
            cached_at: Instant::now(),
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
