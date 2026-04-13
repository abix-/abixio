//! Write-ahead log: fast append, background materialize to file tier.
//!
//! Replaces both the log store (permanent log with GC problem) and the
//! write pool (pre-opened temp files with slot management). Objects below
//! the WAL threshold are appended to a segment file, acked, and later
//! materialized to shard.dat + meta.json by a background worker. Objects
//! above the threshold go straight to the file tier.
//!
//! The WAL is ephemeral -- entries are deleted after materialization.
//! No GC, no compaction, no permanent in-memory index.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::metadata::{ObjectMeta, ObjectMetaFile};
use super::needle::{self, Needle, NeedleLocation};
use super::pathing;
use super::segment::{ActiveSegment, SealedSegment};
use super::StorageError;

/// Default WAL threshold: objects <= 64KB go through the WAL.
pub const DEFAULT_WAL_THRESHOLD: usize = 64 * 1024;

/// An un-materialized entry in the WAL. Read paths use this to serve
/// objects that haven't been written to their final file-per-object
/// location yet.
#[derive(Debug, Clone, Copy)]
pub struct WalEntry {
    pub segment_id: u32,
    pub data_offset: u32,
    pub data_len: u32,
    pub meta_offset: u32,
    pub meta_len: u16,
    pub created_at: u32,
}

/// Lightweight request sent to the background materialize worker.
/// Contains only identity and offsets -- no data copies. The worker
/// reads data from the segment mmap via the shared WAL reference.
pub struct MaterializeRequest {
    pub bucket: String,
    pub key: String,
    pub entry: WalEntry,
}

/// The write-ahead log for one disk volume.
pub struct Wal {
    log_dir: PathBuf,
    active: ActiveSegment,
    sealed: Vec<SealedSegment>,
    /// Un-materialized entries. Keyed by (bucket, key).
    pending: HashMap<(Arc<str>, Arc<str>), WalEntry>,
    /// Per-segment count of un-materialized entries. When a segment
    /// reaches zero, it can be deleted.
    segment_pending_counts: HashMap<u32, u32>,
    next_segment_id: u32,
    segment_capacity: usize,
}

impl Wal {
    /// Open or create a WAL in the given directory.
    /// On startup, scans existing segments for un-materialized entries.
    pub fn open(log_dir: &Path, segment_capacity: usize, root: &Path) -> Result<Self, StorageError> {
        std::fs::create_dir_all(log_dir)?;

        let mut wal = Self {
            log_dir: log_dir.to_path_buf(),
            active: ActiveSegment::create(log_dir, 1, segment_capacity)?,
            sealed: Vec::new(),
            pending: HashMap::new(),
            segment_pending_counts: HashMap::new(),
            next_segment_id: 2,
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

        // open existing segments and find un-materialized entries
        for path in &segment_paths {
            match SealedSegment::open(path) {
                Ok(seg) => {
                    let seg_id = seg.id();
                    if seg_id >= wal.next_segment_id {
                        wal.next_segment_id = seg_id + 1;
                    }
                    wal.recover_segment(&seg, root);
                    // only keep segments that still have pending entries
                    if wal.segment_pending_counts.get(&seg_id).copied().unwrap_or(0) > 0 {
                        wal.sealed.push(seg);
                    } else {
                        // fully materialized -- delete the segment file
                        let seg_path = seg.path().to_path_buf();
                        drop(seg);
                        let _ = std::fs::remove_file(&seg_path);
                    }
                }
                Err(e) => {
                    tracing::warn!("skipping corrupt WAL segment {:?}: {}", path, e);
                }
            }
        }

        // create a fresh active segment (may overwrite the one from init
        // if we found existing segments with higher IDs)
        let active_id = wal.next_segment_id;
        wal.next_segment_id += 1;
        wal.active = ActiveSegment::create(log_dir, active_id, segment_capacity)?;

        Ok(wal)
    }

    /// Scan a sealed segment and add entries that haven't been materialized
    /// to the pending map. An entry is materialized if its shard.dat exists
    /// at the final destination.
    fn recover_segment(&mut self, seg: &SealedSegment, root: &Path) {
        let seg_id = seg.id();
        seg.scan(|flags, bucket, key, meta_offset, meta_len, data_offset, data_len, _needle_offset| {
            if flags == needle::FLAG_DELETE {
                // tombstones: remove from pending if present
                let idx_key = (Arc::<str>::from(bucket), Arc::<str>::from(key));
                if self.pending.remove(&idx_key).is_some() {
                    // decrement the segment that held the removed entry
                    // (we don't track which segment it was, so skip decrement
                    // -- this is conservative, segment might linger until restart)
                }
                return;
            }

            // check if already materialized
            let dest_exists = pathing::object_shard_path(root, bucket, key)
                .map(|p| p.is_file())
                .unwrap_or(false);

            if dest_exists {
                // already on disk, skip
                return;
            }

            // extract created_at from meta if possible
            let created_at = seg.slice(meta_offset, meta_len)
                .and_then(|meta_bytes| Needle::decode_meta(meta_bytes).ok())
                .map(|m| m.created_at as u32)
                .unwrap_or(0);

            let idx_key = (Arc::<str>::from(bucket), Arc::<str>::from(key));
            self.pending.insert(idx_key, WalEntry {
                segment_id: seg_id,
                data_offset: data_offset as u32,
                data_len: data_len as u32,
                meta_offset: meta_offset as u32,
                meta_len: meta_len as u16,
                created_at,
            });
            *self.segment_pending_counts.entry(seg_id).or_insert(0) += 1;
        }).ok();
    }

    /// Append an object to the WAL. Returns the entry for the pending map.
    /// Caller must send a MaterializeRequest to the worker after this.
    pub fn append(&mut self, needle: &Needle) -> Result<WalEntry, StorageError> {
        // try appending to active segment
        if let Some(loc) = self.active.append(needle)? {
            let entry = loc_to_entry(self.active.id(), &loc);
            let idx_key = (Arc::from(needle.bucket.as_str()), Arc::from(needle.key.as_str()));
            self.pending.insert(idx_key, entry);
            *self.segment_pending_counts.entry(self.active.id()).or_insert(0) += 1;
            return Ok(entry);
        }

        // active segment full -- rotate and retry
        self.rotate_active()?;

        if let Some(loc) = self.active.append(needle)? {
            let entry = loc_to_entry(self.active.id(), &loc);
            let idx_key = (Arc::from(needle.bucket.as_str()), Arc::from(needle.key.as_str()));
            self.pending.insert(idx_key, entry);
            *self.segment_pending_counts.entry(self.active.id()).or_insert(0) += 1;
            return Ok(entry);
        }

        Err(StorageError::Internal("needle too large for WAL segment".into()))
    }

    /// Seal the active segment and create a new one.
    fn rotate_active(&mut self) -> Result<(), StorageError> {
        let old_active = std::mem::replace(
            &mut self.active,
            ActiveSegment::create(&self.log_dir, 0, self.segment_capacity)?,
        );
        let sealed = old_active.seal()?;
        self.sealed.push(sealed);

        let seg_id = self.next_segment_id;
        self.next_segment_id += 1;
        self.active = ActiveSegment::create(&self.log_dir, seg_id, self.segment_capacity)?;
        Ok(())
    }

    /// Look up an un-materialized entry by bucket + key.
    pub fn get(&self, bucket: &str, key: &str) -> Option<&WalEntry> {
        let idx_key = (Arc::<str>::from(bucket), Arc::<str>::from(key));
        self.pending.get(&idx_key)
    }

    /// Check if a key exists in the WAL pending map.
    pub fn contains(&self, bucket: &str, key: &str) -> bool {
        let idx_key = (Arc::<str>::from(bucket), Arc::<str>::from(key));
        self.pending.contains_key(&idx_key)
    }

    /// Read shard data for a pending entry. Checks active segment first
    /// (page-cache coherent mmap), then sealed segments.
    pub fn read_data(&self, entry: &WalEntry) -> Option<Vec<u8>> {
        // check active segment
        if entry.segment_id == self.active.id() {
            return self.active.read_at(entry.data_offset as usize, entry.data_len as usize).ok();
        }
        // check sealed segments
        if let Some(seg) = self.sealed.iter().find(|s| s.id() == entry.segment_id) {
            return seg.slice(entry.data_offset as usize, entry.data_len as usize)
                .map(|s| s.to_vec());
        }
        None
    }

    /// Read and decode ObjectMeta for a pending entry.
    pub fn read_meta(&self, entry: &WalEntry) -> Result<ObjectMeta, StorageError> {
        let meta_bytes = self.read_meta_bytes(entry)
            .ok_or(StorageError::ObjectNotFound)?;
        Needle::decode_meta(&meta_bytes)
    }

    /// Read raw meta bytes for a pending entry.
    fn read_meta_bytes(&self, entry: &WalEntry) -> Option<Vec<u8>> {
        if entry.segment_id == self.active.id() {
            return self.active.read_at(entry.meta_offset as usize, entry.meta_len as usize).ok();
        }
        if let Some(seg) = self.sealed.iter().find(|s| s.id() == entry.segment_id) {
            return seg.slice(entry.meta_offset as usize, entry.meta_len as usize)
                .map(|s| s.to_vec());
        }
        None
    }

    /// Mark an entry as materialized (called by the worker after writing
    /// final files). Removes from pending map. If the segment is fully
    /// drained, deletes the segment file.
    pub fn mark_materialized(&mut self, bucket: &str, key: &str) {
        let idx_key = (Arc::<str>::from(bucket), Arc::<str>::from(key));
        if let Some(entry) = self.pending.remove(&idx_key) {
            if let Some(count) = self.segment_pending_counts.get_mut(&entry.segment_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.segment_pending_counts.remove(&entry.segment_id);
                    self.try_delete_segment(entry.segment_id);
                }
            }
        }
    }

    /// Delete a fully-drained sealed segment file.
    fn try_delete_segment(&mut self, segment_id: u32) {
        // don't delete the active segment
        if segment_id == self.active.id() {
            return;
        }
        if let Some(idx) = self.sealed.iter().position(|s| s.id() == segment_id) {
            let seg = self.sealed.remove(idx);
            let path = seg.path().to_path_buf();
            drop(seg); // release mmap
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!("failed to delete drained WAL segment {:?}: {}", path, e);
            }
        }
    }

    /// List all keys in the pending map for a given bucket (for LIST operations).
    pub fn list_keys(&self, bucket: &str, prefix: &str) -> Vec<String> {
        self.pending
            .keys()
            .filter(|(b, k)| b.as_ref() == bucket && k.starts_with(prefix))
            .map(|(_, k)| k.to_string())
            .collect()
    }

    /// Number of un-materialized entries.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Build MaterializeRequests for all un-materialized entries.
    /// Used during startup recovery to re-send to the worker.
    pub fn drain_recovery_requests(&self) -> Vec<MaterializeRequest> {
        self.pending
            .iter()
            .map(|((bucket, key), entry)| MaterializeRequest {
                bucket: bucket.to_string(),
                key: key.to_string(),
                entry: *entry,
            })
            .collect()
    }
}

// SAFETY: Wal uses no interior mutability that would be unsound across threads.
// Access is serialized by the caller (LocalVolume holds &mut or Mutex).
unsafe impl Send for Wal {}

fn loc_to_entry(segment_id: u32, loc: &NeedleLocation) -> WalEntry {
    WalEntry {
        segment_id,
        data_offset: loc.data_offset,
        data_len: loc.data_len,
        meta_offset: loc.meta_offset,
        meta_len: loc.meta_len,
        created_at: loc.created_at,
    }
}

// -------------------------------------------------------------------
// Materialize worker
// -------------------------------------------------------------------

/// Round-robin dispatcher for materialize requests across N workers.
/// Same pattern as the pool's RenameDispatch.
pub struct MaterializeDispatch {
    senders: Vec<tokio::sync::mpsc::Sender<MaterializeRequest>>,
    next: std::sync::atomic::AtomicUsize,
}

impl MaterializeDispatch {
    pub fn new(senders: Vec<tokio::sync::mpsc::Sender<MaterializeRequest>>) -> Self {
        Self {
            senders,
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Send a materialize request to the next worker in round-robin order.
    pub async fn send(
        &self,
        req: MaterializeRequest,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<MaterializeRequest>> {
        let idx = self
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.senders.len();
        self.senders[idx].send(req).await
    }
}

/// Process a single materialize request: read data + meta from the
/// WAL segment mmap, write shard.dat + meta.json to final location.
/// All heavy work (mmap read, JSON serialize, disk write) happens
/// here on the worker, not on the PUT hot path.
async fn process_materialize(
    root: &Path,
    wal: &std::sync::Mutex<Wal>,
    req: &MaterializeRequest,
) -> Result<(), StorageError> {
    // read data and meta from WAL segment (zero-copy mmap slice -> Vec copy)
    let (data, meta_json) = {
        let w = wal.lock().map_err(|e| {
            StorageError::Internal(format!("wal lock in worker: {}", e))
        })?;
        let data = w.read_data(&req.entry)
            .ok_or_else(|| StorageError::Internal("WAL segment data missing".into()))?;
        let meta = w.read_meta(&req.entry)?;
        let mut version = meta;
        version.is_latest = true;
        let mf = ObjectMetaFile {
            bucket: req.bucket.clone(),
            key: req.key.clone(),
            versions: vec![version],
        };
        // simd-json: 30% faster than serde_json (perf win #6)
        let meta_json = simd_json::serde::to_vec(&mf).map_err(|e| {
            StorageError::Internal(format!("simd_json: {}", e))
        })?;
        (data, meta_json)
    };

    // single path computation (perf win #8)
    let dest_dir = pathing::object_dir(root, &req.bucket, &req.key)?;
    let data_dest = dest_dir.join("shard.dat");
    let meta_dest = dest_dir.join("meta.json");

    tokio::fs::create_dir_all(&dest_dir).await?;

    // concurrent data+meta writes (perf win #7)
    tokio::try_join!(
        tokio::fs::write(&data_dest, &data),
        tokio::fs::write(&meta_dest, &meta_json),
    )?;

    Ok(())
}

/// Run a materialize worker until shutdown. Drains the channel on
/// shutdown so all acked PUTs reach their final destination.
pub async fn run_materialize_worker(
    root: PathBuf,
    wal: Arc<std::sync::Mutex<Wal>>,
    mut rx: tokio::sync::mpsc::Receiver<MaterializeRequest>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                // drain remaining items (perf win #4)
                let mut count = 0u32;
                while let Ok(req) = rx.try_recv() {
                    if let Err(e) = process_materialize(&root, &wal, &req).await {
                        tracing::warn!(
                            bucket = %req.bucket, key = %req.key,
                            error = %e, "materialize failed during shutdown drain"
                        );
                        continue;
                    }
                    if let Ok(mut w) = wal.lock() {
                        w.mark_materialized(&req.bucket, &req.key);
                    }
                    count += 1;
                }
                if count > 0 {
                    tracing::info!(count, "WAL worker drained remaining requests on shutdown");
                }
                break;
            }
            msg = rx.recv() => {
                let Some(req) = msg else { break; };
                if let Err(e) = process_materialize(&root, &wal, &req).await {
                    tracing::warn!(
                        bucket = %req.bucket, key = %req.key,
                        error = %e, "materialize failed"
                    );
                    continue;
                }
                if let Ok(mut w) = wal.lock() {
                    w.mark_materialized(&req.bucket, &req.key);
                }
            }
        }
    }
}

// -------------------------------------------------------------------
// Startup recovery
// -------------------------------------------------------------------

/// Recover un-materialized WAL entries on startup. Sends all pending
/// entries to the materialize worker.
pub async fn recover_wal(
    wal: &std::sync::Mutex<Wal>,
    dispatch: &MaterializeDispatch,
) -> u32 {
    let requests = {
        let w = match wal.lock() {
            Ok(w) => w,
            Err(_) => return 0,
        };
        w.drain_recovery_requests()
    };

    let count = requests.len() as u32;
    for req in requests {
        if let Err(e) = dispatch.send(req).await {
            tracing::warn!("failed to send recovery request: {}", e);
        }
    }

    if count > 0 {
        tracing::info!(count, "WAL recovery: sent un-materialized entries to worker");
    }
    count
}

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
    fn append_and_read() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        let root = tmp.path();
        let mut wal = Wal::open(&wal_dir, 1024 * 1024, root).unwrap();

        let needle = Needle::new("bucket", "key.txt", &test_meta(), b"hello world").unwrap();
        let entry = wal.append(&needle).unwrap();

        assert!(wal.contains("bucket", "key.txt"));
        let got = wal.get("bucket", "key.txt").unwrap();
        assert_eq!(got.segment_id, entry.segment_id);

        let data = wal.read_data(got).unwrap();
        assert_eq!(data, b"hello world");

        let meta = wal.read_meta(got).unwrap();
        assert_eq!(meta.size, 4096);
    }

    #[test]
    fn overwrite_updates_pending() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        let mut wal = Wal::open(&wal_dir, 1024 * 1024, tmp.path()).unwrap();

        let n1 = Needle::new("b", "k", &test_meta(), b"v1").unwrap();
        wal.append(&n1).unwrap();

        let n2 = Needle::new("b", "k", &test_meta(), b"v2").unwrap();
        wal.append(&n2).unwrap();

        let entry = wal.get("b", "k").unwrap();
        let data = wal.read_data(entry).unwrap();
        assert_eq!(data, b"v2");
    }

    #[test]
    fn mark_materialized_removes_entry() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        let mut wal = Wal::open(&wal_dir, 1024 * 1024, tmp.path()).unwrap();

        let needle = Needle::new("b", "k", &test_meta(), b"data").unwrap();
        wal.append(&needle).unwrap();
        assert!(wal.contains("b", "k"));

        wal.mark_materialized("b", "k");
        assert!(!wal.contains("b", "k"));
        assert_eq!(wal.pending_count(), 0);
    }

    #[test]
    fn segment_rotation() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        // tiny segments to force rotation
        let mut wal = Wal::open(&wal_dir, 512, tmp.path()).unwrap();

        let data = vec![0x42u8; 100];
        for i in 0..10 {
            let needle = Needle::new("b", &format!("k{}", i), &test_meta(), &data).unwrap();
            wal.append(&needle).unwrap();
        }

        // should have rotated -- sealed segments exist
        assert!(!wal.sealed.is_empty());
        assert_eq!(wal.pending_count(), 10);

        // all entries should be readable
        for i in 0..10 {
            let entry = wal.get("b", &format!("k{}", i)).unwrap();
            let got = wal.read_data(entry).unwrap();
            assert_eq!(got, data);
        }
    }

    #[test]
    fn recovery_skips_materialized() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        let root = tmp.path();

        // write an object to the WAL
        {
            let mut wal = Wal::open(&wal_dir, 1024 * 1024, root).unwrap();
            let needle = Needle::new("bucket", "key", &test_meta(), b"data").unwrap();
            wal.append(&needle).unwrap();
            // simulate materialization: write shard.dat to final location
            let dest_dir = pathing::object_dir(root, "bucket", "key").unwrap();
            std::fs::create_dir_all(&dest_dir).unwrap();
            std::fs::write(dest_dir.join("shard.dat"), b"data").unwrap();
        }
        // drop WAL, simulate restart

        // reopen -- should find no pending entries (already materialized)
        let wal = Wal::open(&wal_dir, 1024 * 1024, root).unwrap();
        assert_eq!(wal.pending_count(), 0);
    }

    #[test]
    fn recovery_re_adds_unmaterialized() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        let root = tmp.path();

        // write objects to WAL, do NOT materialize
        {
            let mut wal = Wal::open(&wal_dir, 1024 * 1024, root).unwrap();
            for i in 0..3 {
                let needle = Needle::new("b", &format!("k{}", i), &test_meta(), b"data").unwrap();
                wal.append(&needle).unwrap();
            }
            // make bucket dir so pathing works but don't write shard files
            let bucket_dir = root.join("b");
            std::fs::create_dir_all(&bucket_dir).unwrap();
        }

        // reopen -- all 3 should be pending
        let wal = Wal::open(&wal_dir, 1024 * 1024, root).unwrap();
        assert_eq!(wal.pending_count(), 3);
        assert!(wal.contains("b", "k0"));
        assert!(wal.contains("b", "k1"));
        assert!(wal.contains("b", "k2"));
    }

    #[test]
    fn list_keys_with_prefix() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        let mut wal = Wal::open(&wal_dir, 1024 * 1024, tmp.path()).unwrap();

        let n1 = Needle::new("b", "photos/a.jpg", &test_meta(), b"a").unwrap();
        let n2 = Needle::new("b", "photos/b.jpg", &test_meta(), b"b").unwrap();
        let n3 = Needle::new("b", "docs/readme.md", &test_meta(), b"c").unwrap();
        wal.append(&n1).unwrap();
        wal.append(&n2).unwrap();
        wal.append(&n3).unwrap();

        let mut keys = wal.list_keys("b", "photos/");
        keys.sort();
        assert_eq!(keys, vec!["photos/a.jpg", "photos/b.jpg"]);
    }

    #[tokio::test]
    async fn materialize_worker_writes_files() {
        let tmp = TempDir::new().unwrap();
        let wal_dir = tmp.path().join("wal");
        let root = tmp.path();
        let wal = Arc::new(std::sync::Mutex::new(
            Wal::open(&wal_dir, 1024 * 1024, root).unwrap()
        ));

        // append an entry
        let meta = test_meta();
        let entry = {
            let needle = Needle::new("b", "k", &meta, b"hello").unwrap();
            wal.lock().unwrap().append(&needle).unwrap()
        };

        // set up worker
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let worker_root = root.to_path_buf();
        let worker_wal = Arc::clone(&wal);
        let handle = tokio::spawn(run_materialize_worker(
            worker_root, worker_wal, rx, shutdown_rx,
        ));

        // make bucket dir
        std::fs::create_dir_all(root.join("b")).unwrap();

        // send lightweight materialize request (worker reads from mmap)
        tx.send(MaterializeRequest {
            bucket: "b".to_string(),
            key: "k".to_string(),
            entry,
        }).await.unwrap();

        // drop sender to close channel, wait for worker
        drop(tx);
        handle.await.unwrap();

        // verify files exist
        let shard_path = pathing::object_shard_path(root, "b", "k").unwrap();
        let data = std::fs::read(&shard_path).unwrap();
        assert_eq!(data, b"hello");

        // verify pending is cleared
        assert_eq!(wal.lock().unwrap().pending_count(), 0);
    }
}
