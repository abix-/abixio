//! LRU read cache for hot small-object GETs.
//!
//! Holds post-decode object bytes keyed by `(bucket, key)`. Sits
//! between the WAL pending-map lookup and the file-tier read on the
//! GET hot path. Multipart and versioned reads bypass the cache.
//!
//! Eviction is strict LRU by total cached bytes. No frequency
//! tracking (upgrade once hit-rate metrics exist). Single `Mutex`
//! guards the map: the hot path is a clone of a ref-counted `Bytes`
//! plus `ObjectInfo`, so lock hold time is microseconds.

use std::sync::Mutex;

use bytes::Bytes;
use lru::LruCache;

use super::metadata::ObjectInfo;

type CacheKey = (String, String);

#[derive(Clone)]
pub struct CacheEntry {
    pub data: Bytes,
    pub info: ObjectInfo,
}

impl CacheEntry {
    fn mem_size(&self) -> u64 {
        let s = &self.info;
        let meta_overhead = s.bucket.len()
            + s.key.len()
            + s.etag.len()
            + s.content_type.len()
            + s.version_id.len()
            + 128;
        (self.data.len() + meta_overhead) as u64
    }
}

pub struct ReadCache {
    inner: Mutex<Inner>,
    max_bytes: u64,
    max_object_bytes: u64,
}

struct Inner {
    map: LruCache<CacheKey, CacheEntry>,
    size_bytes: u64,
}

impl ReadCache {
    pub fn new(max_bytes: u64, max_object_bytes: u64) -> Self {
        Self {
            // Cap entries by bytes, not count. `LruCache::unbounded` has
            // no entry limit; `pop_lru` enforces the byte budget below.
            inner: Mutex::new(Inner {
                map: LruCache::unbounded(),
                size_bytes: 0,
            }),
            max_bytes,
            max_object_bytes,
        }
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub fn max_object_bytes(&self) -> u64 {
        self.max_object_bytes
    }

    /// Look up an entry. Touches the LRU position.
    pub fn get(&self, bucket: &str, key: &str) -> Option<CacheEntry> {
        let mut g = self.inner.lock().ok()?;
        g.map.get(&(bucket.to_string(), key.to_string())).cloned()
    }

    /// Insert an entry. Skips objects larger than `max_object_bytes`.
    /// Evicts LRU entries until `size_bytes <= max_bytes`.
    pub fn put(&self, bucket: &str, key: &str, entry: CacheEntry) {
        if entry.data.len() as u64 > self.max_object_bytes || self.max_bytes == 0 {
            return;
        }
        let entry_size = entry.mem_size();
        let Ok(mut g) = self.inner.lock() else { return };
        let k = (bucket.to_string(), key.to_string());
        if let Some(old) = g.map.pop(&k) {
            g.size_bytes = g.size_bytes.saturating_sub(old.mem_size());
        }
        g.map.put(k, entry);
        g.size_bytes += entry_size;
        while g.size_bytes > self.max_bytes {
            match g.map.pop_lru() {
                Some((_, e)) => {
                    g.size_bytes = g.size_bytes.saturating_sub(e.mem_size());
                }
                None => break,
            }
        }
    }

    /// Remove an entry if present. Called on DELETE and on PUT
    /// overwrites so stale bytes never serve a subsequent GET.
    pub fn invalidate(&self, bucket: &str, key: &str) {
        let Ok(mut g) = self.inner.lock() else { return };
        if let Some(old) = g.map.pop(&(bucket.to_string(), key.to_string())) {
            g.size_bytes = g.size_bytes.saturating_sub(old.mem_size());
        }
    }

    /// Drop every entry under a bucket. Used on DeleteBucket.
    pub fn invalidate_bucket(&self, bucket: &str) {
        let Ok(mut g) = self.inner.lock() else { return };
        let keys: Vec<CacheKey> = g
            .map
            .iter()
            .filter(|(k, _)| k.0 == bucket)
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            if let Some(old) = g.map.pop(&k) {
                g.size_bytes = g.size_bytes.saturating_sub(old.mem_size());
            }
        }
    }

    pub fn size_bytes(&self) -> u64 {
        self.inner.lock().map(|g| g.size_bytes).unwrap_or(0)
    }

    pub fn entry_count(&self) -> usize {
        self.inner.lock().map(|g| g.map.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::metadata::ObjectInfo;

    fn info(bucket: &str, key: &str, size: u64) -> ObjectInfo {
        ObjectInfo {
            bucket: bucket.to_string(),
            key: key.to_string(),
            size,
            etag: "etag".to_string(),
            content_type: "application/octet-stream".to_string(),
            created_at: 0,
            user_metadata: Default::default(),
            tags: Default::default(),
            version_id: String::new(),
            is_delete_marker: false,
        }
    }

    fn entry(bucket: &str, key: &str, size: usize) -> CacheEntry {
        CacheEntry {
            data: Bytes::from(vec![0xa5u8; size]),
            info: info(bucket, key, size as u64),
        }
    }

    #[test]
    fn put_get_round_trip() {
        let c = ReadCache::new(1024 * 1024, 64 * 1024);
        c.put("b", "k", entry("b", "k", 100));
        let got = c.get("b", "k").unwrap();
        assert_eq!(got.data.len(), 100);
        assert_eq!(c.entry_count(), 1);
    }

    #[test]
    fn size_based_eviction() {
        // 200 bytes of budget. Each entry is ~228 bytes with the
        // overhead constant (128) + small strings; tune this test
        // around that. Three entries must force at least one eviction.
        let c = ReadCache::new(500, 64 * 1024);
        c.put("b", "a", entry("b", "a", 50));
        c.put("b", "b", entry("b", "b", 50));
        c.put("b", "c", entry("b", "c", 50));
        c.put("b", "d", entry("b", "d", 50));
        assert!(c.size_bytes() <= 500);
        assert!(c.entry_count() < 4);
    }

    #[test]
    fn oversized_entry_skipped() {
        let c = ReadCache::new(10 * 1024, 4 * 1024);
        c.put("b", "big", entry("b", "big", 8 * 1024));
        assert_eq!(c.entry_count(), 0);
    }

    #[test]
    fn invalidate_removes_entry() {
        let c = ReadCache::new(1024 * 1024, 64 * 1024);
        c.put("b", "k", entry("b", "k", 100));
        assert!(c.get("b", "k").is_some());
        c.invalidate("b", "k");
        assert!(c.get("b", "k").is_none());
        assert_eq!(c.size_bytes(), 0);
    }

    #[test]
    fn overwrite_keeps_one_copy() {
        let c = ReadCache::new(1024 * 1024, 64 * 1024);
        c.put("b", "k", entry("b", "k", 100));
        c.put("b", "k", entry("b", "k", 200));
        let e = c.get("b", "k").unwrap();
        assert_eq!(e.data.len(), 200);
        assert_eq!(c.entry_count(), 1);
    }

    #[test]
    fn invalidate_bucket_drops_all_under_bucket() {
        let c = ReadCache::new(1024 * 1024, 64 * 1024);
        c.put("b", "k1", entry("b", "k1", 100));
        c.put("b", "k2", entry("b", "k2", 100));
        c.put("other", "k", entry("other", "k", 100));
        c.invalidate_bucket("b");
        assert!(c.get("b", "k1").is_none());
        assert!(c.get("b", "k2").is_none());
        assert!(c.get("other", "k").is_some());
    }

    #[test]
    fn lru_recency_prefers_recently_used() {
        // Budget sized for roughly 3 of these entries. After touching
        // "a", the next insert must evict "b" (now oldest) before
        // touching "a".
        let c = ReadCache::new(700, 64 * 1024);
        c.put("b", "a", entry("b", "a", 50));
        c.put("b", "b", entry("b", "b", 50));
        c.put("b", "c", entry("b", "c", 50));
        // touch a so b is now the oldest
        let _ = c.get("b", "a");
        // this insert should push the total over budget and evict b
        c.put("b", "d", entry("b", "d", 50));
        assert!(c.get("b", "a").is_some(), "a was touched, should survive");
        assert!(c.get("b", "b").is_none(), "b should be the first evicted");
    }

    #[test]
    fn zero_max_disables_cache() {
        let c = ReadCache::new(0, 64 * 1024);
        c.put("b", "k", entry("b", "k", 10));
        assert_eq!(c.entry_count(), 0);
    }
}
