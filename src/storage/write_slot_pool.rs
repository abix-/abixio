//! Pre-opened temp file pool primitive.
//!
//! Holds a fixed-size pool of slot pairs (data + meta files) ready
//! for the PUT path to consume. This module owns ONLY the pool
//! plumbing -- no rename worker, no pending_renames table, no
//! integration with LocalVolume. Those land in later phases.
//!
//! The point of pre-opening is to remove file-create syscalls from
//! the PUT hot path. A consumer pops a slot, writes data and meta
//! to the already-open files, and ack-s. The actual rename to the
//! destination happens on a background worker (Phase 3+); this
//! module's `release` is the test-harness stand-in for what the
//! worker will do later.
//!
//! See docs/write-pool.md for the full design.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use dashmap::DashMap;
use tokio::fs::File;

use super::metadata::read_meta_file;
use super::pathing;
use super::StorageError;

/// In-memory record of a PUT that has been written to a slot but
/// whose temp files haven't been renamed to their final destination
/// yet. Read paths consult `PendingRenames` (a DashMap of these)
/// before falling through to the file tier so that read-after-write
/// is consistent.
///
/// Phase 5.5 trimmed this struct to just the fields the read paths
/// actually need to find the temp files. The full `ObjectMeta` is no
/// longer cloned in here -- read/stat paths read the temp meta file
/// instead, which is cheap because it was just written through the
/// page cache. Dropping the meta clone removed ~17us from the 4KB
/// PUT hot path.
#[derive(Debug, Clone)]
pub struct PendingEntry {
    pub slot_id: u32,
    pub data_path: PathBuf,
    pub meta_path: PathBuf,
    pub data_len: u64,
}

/// Shared in-memory table mapping `(bucket, key)` to a `PendingEntry`.
/// `LocalVolume` and the rename worker share an `Arc<DashMap>` so
/// readers can find pending objects and the worker can remove them
/// after each rename completes.
pub type PendingRenames =
    Arc<DashMap<(Arc<str>, Arc<str>), PendingEntry>>;

/// One slot in the pool: a pair of pre-opened files ready for a PUT.
///
/// `data_file` receives shard bytes; `meta_file` receives the meta JSON.
/// Both are opened with create+write+truncate so they start empty and
/// can accept arbitrary content.
pub struct WriteSlot {
    pub slot_id: u32,
    pub data_file: File,
    pub meta_file: File,
    pub data_path: PathBuf,
    pub meta_path: PathBuf,
}

/// Lock-free pool of pre-opened slots.
///
/// Backed by a `crossbeam_queue::ArrayQueue` (bounded MPMC, CAS-based).
/// Pop and push are sub-microsecond and scale linearly under contention.
pub struct WriteSlotPool {
    pool_dir: PathBuf,
    slots: ArrayQueue<WriteSlot>,
    depth: u32,
}

impl WriteSlotPool {
    /// Create a fresh pool with `depth` slot pairs in `pool_dir`.
    ///
    /// The directory is created if it doesn't exist. Existing files
    /// inside are NOT touched -- crash recovery handles those in a
    /// later phase.
    pub async fn new(pool_dir: &Path, depth: u32) -> Result<Arc<Self>, StorageError> {
        tokio::fs::create_dir_all(pool_dir).await?;

        let queue = ArrayQueue::new(depth as usize);
        for slot_id in 0..depth {
            let slot = create_slot(pool_dir, slot_id).await?;
            queue
                .push(slot)
                .map_err(|_| StorageError::Internal("pool queue push failed during init".into()))?;
        }

        Ok(Arc::new(Self {
            pool_dir: pool_dir.to_path_buf(),
            slots: queue,
            depth,
        }))
    }

    /// Try to grab a slot. Returns None if the pool is empty -- caller
    /// should fall back to the slow path. Never blocks.
    pub fn try_pop(&self) -> Option<WriteSlot> {
        self.slots.pop()
    }

    /// Return a slot to the pool. In the real flow this is called by
    /// the rename worker after the rename completes; in this phase
    /// the test harness calls it directly.
    pub fn release(&self, slot: WriteSlot) -> Result<(), StorageError> {
        self.slots
            .push(slot)
            .map_err(|_| StorageError::Internal("pool queue full on release".into()))
    }

    /// Current number of available slots.
    pub fn available(&self) -> usize {
        self.slots.len()
    }

    /// Configured pool depth.
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Pool directory.
    pub fn pool_dir(&self) -> &Path {
        &self.pool_dir
    }

    /// Replenish a consumed slot by creating fresh files at the
    /// slot_id paths and pushing the new `WriteSlot` back into the
    /// queue. Called by the rename worker after a successful rename.
    pub async fn replenish_slot(&self, slot_id: u32) -> Result<(), StorageError> {
        let slot = create_slot(&self.pool_dir, slot_id).await?;
        self.slots
            .push(slot)
            .map_err(|_| StorageError::Internal("pool queue full on replenish".into()))
    }
}

/// A request to finalize a slot by renaming its files to the
/// destination paths. Sent on the rename channel by the PUT path;
/// consumed by `run_rename_worker`.
///
/// `bucket` and `key` are carried so the worker can remove the
/// matching `PendingRenames` entry after the rename completes.
///
/// Phase 5.6: the request now owns the entire `WriteSlot` (including
/// the open file handles). The PUT path passes the slot in instead
/// of dropping the handles itself, which moves the slow file-close
/// operations off the hot path and onto the rename worker.
///
/// `WriteSlot` doesn't implement `Debug` (it contains live tokio
/// `File`s), so neither does `RenameRequest`.
pub struct RenameRequest {
    pub slot: WriteSlot,
    pub bucket: String,
    pub key: String,
    pub dest_dir: PathBuf,
    pub data_dest: PathBuf,
    pub meta_dest: PathBuf,
}

/// Round-robin dispatcher that fans rename requests out across N
/// worker channels. Phase 8.7 scales the pool's rename drain rate
/// by running multiple workers in parallel instead of one.
///
/// Each worker owns its own `mpsc::Receiver`, so workers don't
/// fight over a shared queue. The producer picks a sender via an
/// atomic round-robin counter, which means with N workers and a
/// per-worker channel buffer B, the effective total buffer before
/// `send` blocks is N * B, and the effective rename throughput is
/// ~N times a single worker (Phase 3 measured 1.85x at 2 workers).
pub struct RenameDispatch {
    senders: Vec<tokio::sync::mpsc::Sender<RenameRequest>>,
    next: std::sync::atomic::AtomicUsize,
}

impl RenameDispatch {
    pub fn new(senders: Vec<tokio::sync::mpsc::Sender<RenameRequest>>) -> Self {
        Self {
            senders,
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Send a rename request to the next worker in round-robin order.
    /// Awaits if the chosen worker's channel is full (backpressure).
    pub async fn send(
        &self,
        req: RenameRequest,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<RenameRequest>> {
        let idx = self
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.senders.len();
        self.senders[idx].send(req).await
    }

    pub fn worker_count(&self) -> usize {
        self.senders.len()
    }
}

/// Process a single rename request: drop the file handles (off the
/// hot path), mkdir destination, two atomic renames, then **always**
/// replenish the slot. The replenish happens regardless of whether
/// the renames succeeded -- if the renames fail (e.g. user cancellation
/// deleted the temp files first), the slot would otherwise leak.
///
/// Phase 5.6: takes the request by value so it can move the slot out
/// and drop the file handles here on the worker thread, instead of
/// in the PUT request handler.
///
/// Public so the bench harness can invoke it directly when measuring
/// parallel-worker scaling.
pub async fn process_rename_request(
    pool: &WriteSlotPool,
    req: RenameRequest,
) -> Result<(), StorageError> {
    let RenameRequest {
        slot,
        dest_dir,
        data_dest,
        meta_dest,
        ..
    } = req;
    let WriteSlot {
        slot_id,
        data_file,
        meta_file,
        data_path,
        meta_path,
    } = slot;

    // Drop the file handles on the worker thread. On Windows, file
    // close occasionally takes ~1.5ms (kernel close + last-write-time
    // bookkeeping). Doing this in the request handler used to add
    // ~65us avg / 1.57ms p99 to the hot path; doing it here keeps the
    // hot path tight at the cost of a slightly slower worker drain.
    drop(data_file);
    drop(meta_file);

    let rename_result = async {
        tokio::fs::create_dir_all(&dest_dir).await?;
        tokio::fs::rename(&data_path, &data_dest).await?;
        tokio::fs::rename(&meta_path, &meta_dest).await?;
        Ok::<(), StorageError>(())
    }
    .await;
    let replenish_result = pool.replenish_slot(slot_id).await;
    rename_result.and(replenish_result)
}

/// Run the rename worker until the channel closes or `shutdown`
/// fires. Errors are logged via `tracing` and the worker continues
/// -- a single failed rename should not kill the worker.
///
/// If `pending` is `Some`, after each successful rename the worker
/// removes the matching `(bucket, key)` entry so the read path
/// stops consulting it. If `None`, the worker only does the renames
/// (used by the Phase 3 microbenches that don't need a pending table).
///
/// Matches the existing heal worker shutdown pattern at
/// `src/heal/worker.rs:181-226`.
pub async fn run_rename_worker(
    pool: Arc<WriteSlotPool>,
    pending: Option<PendingRenames>,
    mut rx: tokio::sync::mpsc::Receiver<RenameRequest>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                // drain remaining items in the channel before exiting
                // so acked PUTs reach their final destination
                drain_remaining(&pool, &pending, &mut rx).await;
                break;
            }
            msg = rx.recv() => {
                let Some(req) = msg else { break; };
                process_one(&pool, &pending, req).await;
            }
        }
    }
}

async fn drain_remaining(
    pool: &Arc<WriteSlotPool>,
    pending: &Option<PendingRenames>,
    rx: &mut tokio::sync::mpsc::Receiver<RenameRequest>,
) {
    let mut count = 0u32;
    while let Ok(req) = rx.try_recv() {
        process_one(pool, pending, req).await;
        count += 1;
    }
    if count > 0 {
        tracing::info!(count, "rename worker drained remaining requests on shutdown");
    }
}

async fn process_one(
    pool: &Arc<WriteSlotPool>,
    pending: &Option<PendingRenames>,
    req: RenameRequest,
) {
    let req_bucket = req.bucket.clone();
    let req_key = req.key.clone();
    let req_slot_id = req.slot.slot_id;
    let req_data_src_display = req.slot.data_path.display().to_string();
    let result = process_rename_request(pool, req).await;
    if let Err(ref e) = result {
        tracing::warn!(
            slot_id = req_slot_id,
            data_src = %req_data_src_display,
            error = %e,
            "rename worker request failed",
        );
    }
    if result.is_ok() {
        if let Some(pending) = pending {
            let key = (
                Arc::<str>::from(req_bucket.as_str()),
                Arc::<str>::from(req_key.as_str()),
            );
            pending.remove(&key);
        }
    }
}

/// Create one slot pair on disk and return the open file handles.
async fn create_slot(pool_dir: &Path, slot_id: u32) -> Result<WriteSlot, StorageError> {
    let data_path = pool_dir.join(format!("slot-{:04}.data.tmp", slot_id));
    let meta_path = pool_dir.join(format!("slot-{:04}.meta.tmp", slot_id));
    let data_file = File::create(&data_path).await?;
    let meta_file = File::create(&meta_path).await?;
    Ok(WriteSlot {
        slot_id,
        data_file,
        meta_file,
        data_path,
        meta_path,
    })
}

// -----------------------------------------------------------------------
// Phase 6: crash recovery scan
// -----------------------------------------------------------------------
//
// On process restart, the pool dir may contain slot-N.data.tmp and
// slot-N.meta.tmp files from PUTs that were acked before a crash but
// whose renames to the final destination had not completed. This
// module's `recover_pool_dir` finishes those renames before the fresh
// pool is created. It must run BEFORE `WriteSlotPool::new` because
// that constructor calls `File::create` which truncates existing
// files at the slot-N.*.tmp paths.
//
// The design is stateless: temp meta files carry `bucket` and `key`
// as identity fields (added in Phase 4), so recovery finds each
// pending PUT's destination from the file alone, without any in-RAM
// index. See `docs/write-pool.md` section "Crash recovery: stateless"
// for the full design.

/// What happened to one recovered slot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Both renames ran during recovery (data + meta moved to dest).
    pub recovered_pairs: u32,
    /// Only the meta rename ran (data was already at dest; we crashed
    /// between the two renames).
    pub half_renamed_fixed: u32,
    /// Temp files with no parseable meta or no target destination. The
    /// PUT was never acked, so we deleted them.
    pub orphans_deleted: u32,
    /// Meta files that failed to parse as `ObjectMetaFile`. Deleted
    /// along with any matching data temp.
    pub unparseable_deleted: u32,
}

/// Per-pair recovery outcome.
enum RecoverResult {
    /// Both renames succeeded during recovery.
    Full,
    /// Only the meta rename ran; data was already at dest.
    HalfAlreadyDone,
    /// Could not recover (parse failure, empty identity, invalid
    /// destination, rename error). Caller deletes the temp files.
    Unparseable,
}

/// Grouped state per slot id found in the pool dir.
enum PairState {
    /// Both data and meta temp files present.
    Pair { data_path: PathBuf, meta_path: PathBuf },
    /// Only the meta temp file present. May be half-renamed (data
    /// already at dest) or a true orphan.
    MetaOnly { meta_path: PathBuf },
    /// Only the data temp file present. PUT was never acked.
    DataOnly { data_path: PathBuf },
}

/// Scan `pool_dir` for leftover temp files from a previous crash
/// and finish any renames that can still be completed. Unrecoverable
/// files are deleted. Stray files that don't match the slot naming
/// convention are also deleted. The pool directory is private to
/// the pool.
///
/// Creates `pool_dir` if it doesn't exist. First-ever startup is a
/// no-op and returns a zero report.
///
/// Must run BEFORE `WriteSlotPool::new` because that constructor
/// truncates existing slot-*.tmp files.
pub async fn recover_pool_dir(
    root: &Path,
    pool_dir: &Path,
) -> Result<RecoveryReport, StorageError> {
    tokio::fs::create_dir_all(pool_dir).await?;

    let mut grouped: HashMap<u32, (Option<PathBuf>, Option<PathBuf>)> = HashMap::new();
    let mut stray: Vec<PathBuf> = Vec::new();

    let mut entries = tokio::fs::read_dir(pool_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        // Only regular files matter. Skip subdirs quietly.
        match entry.file_type().await {
            Ok(ft) if ft.is_file() => {}
            _ => continue,
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            stray.push(path);
            continue;
        };
        match classify_slot_filename(name) {
            Some((slot_id, SlotHalf::Data)) => {
                grouped.entry(slot_id).or_default().0 = Some(path);
            }
            Some((slot_id, SlotHalf::Meta)) => {
                grouped.entry(slot_id).or_default().1 = Some(path);
            }
            None => stray.push(path),
        }
    }

    let mut report = RecoveryReport::default();

    // Delete stray files. The pool dir is private to the pool.
    for path in stray {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!(path = %path.display(), error = %e, "failed to delete stray file in pool dir");
        }
    }

    for (_slot_id, (data_opt, meta_opt)) in grouped {
        let state = match (data_opt, meta_opt) {
            (Some(data_path), Some(meta_path)) => PairState::Pair { data_path, meta_path },
            (None, Some(meta_path)) => PairState::MetaOnly { meta_path },
            (Some(data_path), None) => PairState::DataOnly { data_path },
            (None, None) => continue,
        };

        match state {
            PairState::Pair { data_path, meta_path } => {
                match parse_and_recover_pair(root, &data_path, &meta_path).await {
                    RecoverResult::Full => report.recovered_pairs += 1,
                    RecoverResult::HalfAlreadyDone => report.half_renamed_fixed += 1,
                    RecoverResult::Unparseable => {
                        // The PUT was never acked (meta didn't parse,
                        // no destination, or rename failed). Delete
                        // both temps so the fresh pool can claim the
                        // slot id.
                        let _ = tokio::fs::remove_file(&data_path).await;
                        let _ = tokio::fs::remove_file(&meta_path).await;
                        report.unparseable_deleted += 1;
                    }
                }
            }
            PairState::MetaOnly { meta_path } => {
                match recover_meta_only(root, &meta_path).await {
                    RecoverResult::HalfAlreadyDone => report.half_renamed_fixed += 1,
                    _ => {
                        // True orphan: data_dest doesn't exist and
                        // there's no temp data file. The PUT was
                        // never acked. Delete the meta.
                        let _ = tokio::fs::remove_file(&meta_path).await;
                        report.orphans_deleted += 1;
                    }
                }
            }
            PairState::DataOnly { data_path } => {
                // No meta means the PUT was not acked (meta write is
                // part of the pre-ack tokio::try_join!). Delete.
                let _ = tokio::fs::remove_file(&data_path).await;
                report.orphans_deleted += 1;
            }
        }
    }

    Ok(report)
}

#[derive(Clone, Copy)]
enum SlotHalf {
    Data,
    Meta,
}

/// Parse `slot-NNNN.(data|meta).tmp` into (slot_id, half). Returns
/// None for any other filename.
fn classify_slot_filename(name: &str) -> Option<(u32, SlotHalf)> {
    let rest = name.strip_prefix("slot-")?;
    let (digits, tail) = rest.split_at(rest.find('.')?);
    if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let slot_id: u32 = digits.parse().ok()?;
    let half = match tail {
        ".data.tmp" => SlotHalf::Data,
        ".meta.tmp" => SlotHalf::Meta,
        _ => return None,
    };
    Some((slot_id, half))
}

/// Parse the temp meta file, compute the destination from its
/// `bucket` and `key` identity fields, mkdir the destination, and
/// run the two renames. Handles the half-renamed case by skipping
/// the data rename when `data_dest` already exists.
async fn parse_and_recover_pair(
    root: &Path,
    data_path: &Path,
    meta_path: &Path,
) -> RecoverResult {
    let Ok(mf) = read_meta_file(meta_path).await else {
        return RecoverResult::Unparseable;
    };
    if mf.bucket.is_empty() || mf.key.is_empty() {
        return RecoverResult::Unparseable;
    }
    // Phase 6 only supports non-versioned recovery. The pool's write
    // path is non-versioned today.
    let Ok(dest_dir) = pathing::object_dir(root, &mf.bucket, &mf.key) else {
        return RecoverResult::Unparseable;
    };
    let data_dest = dest_dir.join("shard.dat");
    let meta_dest = dest_dir.join("meta.json");

    if tokio::fs::create_dir_all(&dest_dir).await.is_err() {
        return RecoverResult::Unparseable;
    }

    // Half-renamed detection: if shard.dat is already at the dest,
    // we crashed between the two renames. Skip the data rename and
    // finish only the meta rename.
    let data_already_at_dest = tokio::fs::try_exists(&data_dest).await.unwrap_or(false);
    if !data_already_at_dest {
        if tokio::fs::rename(data_path, &data_dest).await.is_err() {
            return RecoverResult::Unparseable;
        }
    } else {
        // Redundant stale temp data. Delete it so nothing blocks
        // the next startup's fresh-slot creation.
        let _ = tokio::fs::remove_file(data_path).await;
    }

    if tokio::fs::rename(meta_path, &meta_dest).await.is_err() {
        return RecoverResult::Unparseable;
    }

    if data_already_at_dest {
        RecoverResult::HalfAlreadyDone
    } else {
        RecoverResult::Full
    }
}

/// Meta-only case. This is only recoverable as half-renamed: the
/// data rename must have already completed and left a shard.dat at
/// the destination. If `data_dest` doesn't exist, there's no way to
/// finish the PUT and the meta is a true orphan.
async fn recover_meta_only(root: &Path, meta_path: &Path) -> RecoverResult {
    let Ok(mf) = read_meta_file(meta_path).await else {
        return RecoverResult::Unparseable;
    };
    if mf.bucket.is_empty() || mf.key.is_empty() {
        return RecoverResult::Unparseable;
    }
    let Ok(dest_dir) = pathing::object_dir(root, &mf.bucket, &mf.key) else {
        return RecoverResult::Unparseable;
    };
    let data_dest = dest_dir.join("shard.dat");
    let meta_dest = dest_dir.join("meta.json");
    if !tokio::fs::try_exists(&data_dest).await.unwrap_or(false) {
        return RecoverResult::Unparseable;
    }
    if tokio::fs::create_dir_all(&dest_dir).await.is_err() {
        return RecoverResult::Unparseable;
    }
    if tokio::fs::rename(meta_path, &meta_dest).await.is_err() {
        return RecoverResult::Unparseable;
    }
    RecoverResult::HalfAlreadyDone
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn pool_init_creates_n_slot_pairs() {
        let tmp = TempDir::new().unwrap();
        let pool = WriteSlotPool::new(tmp.path(), 8).await.unwrap();
        assert_eq!(pool.depth(), 8);
        assert_eq!(pool.available(), 8);

        // verify 16 files actually exist on disk
        let mut count = 0;
        for entry in std::fs::read_dir(tmp.path()).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                count += 1;
            }
        }
        assert_eq!(count, 16, "expected 8 data + 8 meta files");
    }

    #[tokio::test]
    async fn pop_release_cycle_preserves_depth() {
        let tmp = TempDir::new().unwrap();
        let pool = WriteSlotPool::new(tmp.path(), 4).await.unwrap();

        for _ in 0..100 {
            let slot = pool.try_pop().expect("pool should have slots");
            pool.release(slot).unwrap();
        }
        assert_eq!(pool.available(), 4);
    }

    #[tokio::test]
    async fn try_pop_returns_none_when_empty() {
        let tmp = TempDir::new().unwrap();
        let pool = WriteSlotPool::new(tmp.path(), 2).await.unwrap();

        let s1 = pool.try_pop().unwrap();
        let s2 = pool.try_pop().unwrap();
        assert!(pool.try_pop().is_none(), "third pop should return None");

        pool.release(s1).unwrap();
        pool.release(s2).unwrap();
        assert_eq!(pool.available(), 2);
    }

    #[tokio::test]
    async fn slot_files_exist_at_reported_paths() {
        let tmp = TempDir::new().unwrap();
        let pool = WriteSlotPool::new(tmp.path(), 1).await.unwrap();

        let slot = pool.try_pop().unwrap();
        assert!(slot.data_path.exists());
        assert!(slot.meta_path.exists());
        assert!(slot.data_path.to_string_lossy().contains("slot-0000.data.tmp"));
        assert!(slot.meta_path.to_string_lossy().contains("slot-0000.meta.tmp"));
    }

    #[tokio::test]
    async fn replenish_slot_creates_fresh_files_at_same_path() {
        use tokio::io::AsyncWriteExt;
        let tmp = TempDir::new().unwrap();
        let pool = WriteSlotPool::new(tmp.path(), 1).await.unwrap();

        // Pop slot 0, write some data, "consume" it by deleting the files,
        // then replenish. The new slot should appear at the same paths
        // and the files should be fresh (empty).
        let slot = pool.try_pop().unwrap();
        let data_path = slot.data_path.clone();
        let meta_path = slot.meta_path.clone();
        let WriteSlot { mut data_file, mut meta_file, .. } = slot;
        data_file.write_all(b"junk").await.unwrap();
        meta_file.write_all(b"more junk").await.unwrap();
        drop(data_file);
        drop(meta_file);
        std::fs::remove_file(&data_path).unwrap();
        std::fs::remove_file(&meta_path).unwrap();
        assert_eq!(pool.available(), 0);

        pool.replenish_slot(0).await.unwrap();
        assert_eq!(pool.available(), 1);

        let slot = pool.try_pop().unwrap();
        assert_eq!(slot.slot_id, 0);
        assert!(slot.data_path.exists());
        assert!(slot.meta_path.exists());
        // Fresh files should be empty
        let data_meta = std::fs::metadata(&slot.data_path).unwrap();
        let meta_meta = std::fs::metadata(&slot.meta_path).unwrap();
        assert_eq!(data_meta.len(), 0);
        assert_eq!(meta_meta.len(), 0);
    }

    #[tokio::test]
    async fn worker_drains_single_request_and_replenishes_pool() {
        use tokio::io::AsyncWriteExt;
        let tmp = TempDir::new().unwrap();
        let pool_dir = tmp.path().join("pool");
        let dest_dir = tmp.path().join("dest");
        let pool = WriteSlotPool::new(&pool_dir, 1).await.unwrap();

        // Pop the slot, write data, pass the slot in the rename request
        let mut slot = pool.try_pop().unwrap();
        slot.data_file.write_all(b"hello").await.unwrap();
        slot.meta_file.write_all(b"world").await.unwrap();
        assert_eq!(pool.available(), 0);

        let req = RenameRequest {
            slot,
            bucket: "b".to_string(),
            key: "k".to_string(),
            dest_dir: dest_dir.clone(),
            data_dest: dest_dir.join("shard.dat"),
            meta_dest: dest_dir.join("meta.json"),
        };

        // Run the worker via the public function
        let (tx, rx) = tokio::sync::mpsc::channel::<RenameRequest>(8);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let pool_clone = Arc::clone(&pool);
        let handle = tokio::spawn(async move {
            run_rename_worker(pool_clone, None, rx, shutdown_rx).await;
        });

        tx.send(req).await.unwrap();
        drop(tx); // close channel -> worker exits after draining
        handle.await.unwrap();

        // Verify destination files exist with the right contents
        let data = std::fs::read(dest_dir.join("shard.dat")).unwrap();
        assert_eq!(data, b"hello");
        let meta = std::fs::read(dest_dir.join("meta.json")).unwrap();
        assert_eq!(meta, b"world");

        // Verify pool is back to depth 1 (replenished)
        assert_eq!(pool.available(), 1);
    }

    #[tokio::test]
    async fn worker_replenishes_slot_even_when_rename_fails() {
        use tokio::io::AsyncWriteExt;
        let tmp = TempDir::new().unwrap();
        let pool_dir = tmp.path().join("pool");
        let dest_dir = tmp.path().join("dest");
        let pool = WriteSlotPool::new(&pool_dir, 2).await.unwrap();

        // Slot 0: bad request (data file removed before send) -- the
        // rename will fail with ENOENT. The replenish should still
        // happen because process_rename_request always replenishes.
        // Slot 1: good request -- should succeed AFTER the failure.
        let slot0 = pool.try_pop().unwrap();
        let mut slot1 = pool.try_pop().unwrap();

        // Remove the data file from disk; the slot's open file handle
        // is still valid (it points to the unlinked file). The rename
        // will fail because the path is gone.
        std::fs::remove_file(&slot0.data_path).unwrap();
        let bad_req = RenameRequest {
            slot: slot0,
            bucket: "b".to_string(),
            key: "a".to_string(),
            dest_dir: dest_dir.clone(),
            data_dest: dest_dir.join("a.dat"),
            meta_dest: dest_dir.join("a.meta"),
        };

        slot1.data_file.write_all(b"good data").await.unwrap();
        slot1.meta_file.write_all(b"good meta").await.unwrap();
        let good_req = RenameRequest {
            slot: slot1,
            bucket: "b".to_string(),
            key: "b".to_string(),
            dest_dir: dest_dir.clone(),
            data_dest: dest_dir.join("b.dat"),
            meta_dest: dest_dir.join("b.meta"),
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<RenameRequest>(8);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let pool_clone = Arc::clone(&pool);
        let handle = tokio::spawn(async move {
            run_rename_worker(pool_clone, None, rx, shutdown_rx).await;
        });

        tx.send(bad_req).await.unwrap();
        tx.send(good_req).await.unwrap();
        drop(tx);
        handle.await.unwrap();

        // Good request should have succeeded despite the bad one
        assert!(dest_dir.join("b.dat").exists());
        assert!(dest_dir.join("b.meta").exists());
        // BOTH slots should be replenished (Phase 5 fix)
        assert_eq!(pool.available(), 2, "both slots should be replenished even after failure");
    }

    #[tokio::test]
    async fn worker_removes_pending_entry_after_successful_rename() {
        use tokio::io::AsyncWriteExt;
        let tmp = TempDir::new().unwrap();
        let pool_dir = tmp.path().join("pool");
        let dest_dir = tmp.path().join("dest");
        let pool = WriteSlotPool::new(&pool_dir, 1).await.unwrap();

        // Build a pending table and pre-populate it like LocalVolume would
        let pending: PendingRenames = Arc::new(DashMap::new());
        let bucket: Arc<str> = Arc::from("b");
        let key: Arc<str> = Arc::from("k");

        let mut slot = pool.try_pop().unwrap();
        let entry = PendingEntry {
            slot_id: slot.slot_id,
            data_path: slot.data_path.clone(),
            meta_path: slot.meta_path.clone(),
            data_len: 2,
        };
        slot.data_file.write_all(b"hi").await.unwrap();
        slot.meta_file.write_all(b"meta").await.unwrap();

        pending.insert((Arc::clone(&bucket), Arc::clone(&key)), entry);
        assert_eq!(pending.len(), 1);

        let req = RenameRequest {
            slot,
            bucket: "b".to_string(),
            key: "k".to_string(),
            dest_dir: dest_dir.clone(),
            data_dest: dest_dir.join("shard.dat"),
            meta_dest: dest_dir.join("meta.json"),
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<RenameRequest>(8);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let pool_clone = Arc::clone(&pool);
        let pending_clone = Arc::clone(&pending);
        let handle = tokio::spawn(async move {
            run_rename_worker(pool_clone, Some(pending_clone), rx, shutdown_rx).await;
        });
        tx.send(req).await.unwrap();
        drop(tx);
        handle.await.unwrap();

        // Worker should have removed the pending entry after rename
        assert_eq!(pending.len(), 0);
        assert!(dest_dir.join("shard.dat").exists());
    }

    // -------------------------------------------------------------
    // Phase 6: crash recovery scan
    // -------------------------------------------------------------

    use crate::storage::metadata::{ErasureMeta, ObjectMeta, ObjectMetaFile};

    /// Write a realistic meta.tmp file with bucket/key populated.
    async fn write_meta_tmp(path: &Path, bucket: &str, key: &str) {
        let mf = ObjectMetaFile {
            bucket: bucket.to_string(),
            key: key.to_string(),
            versions: vec![ObjectMeta {
                size: 5,
                etag: "deadbeef".to_string(),
                content_type: "application/octet-stream".to_string(),
                created_at: 0,
                erasure: ErasureMeta::default(),
                checksum: String::new(),
                ..Default::default()
            }],
        };
        let json = serde_json::to_vec(&mf).unwrap();
        tokio::fs::write(path, json).await.unwrap();
    }

    #[tokio::test]
    async fn recover_full_pair_moves_both_files_to_destination() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        tokio::fs::create_dir_all(&pool_dir).await.unwrap();

        let data_tmp = pool_dir.join("slot-0000.data.tmp");
        let meta_tmp = pool_dir.join("slot-0000.meta.tmp");
        tokio::fs::write(&data_tmp, b"hello").await.unwrap();
        write_meta_tmp(&meta_tmp, "b", "k").await;

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report.recovered_pairs, 1);
        assert_eq!(report.half_renamed_fixed, 0);
        assert_eq!(report.orphans_deleted, 0);
        assert_eq!(report.unparseable_deleted, 0);

        // Both files should now be at the destination.
        let data_dest = root.join("b").join("k").join("shard.dat");
        let meta_dest = root.join("b").join("k").join("meta.json");
        assert!(data_dest.exists());
        assert!(meta_dest.exists());
        assert_eq!(std::fs::read(&data_dest).unwrap(), b"hello");
        // Temp files should be gone.
        assert!(!data_tmp.exists());
        assert!(!meta_tmp.exists());
    }

    #[tokio::test]
    async fn recover_half_renamed_pair_finishes_meta_only() {
        // Simulated crash state: the data rename completed, but the
        // meta rename didn't, AND the data temp is still present
        // (pre-Phase-6 would always delete that; we treat it as
        // stale and drop it, then move the meta).
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        tokio::fs::create_dir_all(&pool_dir).await.unwrap();

        // Pre-place shard.dat at the destination.
        let dest_dir = root.join("b").join("k");
        tokio::fs::create_dir_all(&dest_dir).await.unwrap();
        tokio::fs::write(dest_dir.join("shard.dat"), b"final").await.unwrap();

        // The temp pair still exists (stale data.tmp + real meta.tmp).
        let data_tmp = pool_dir.join("slot-0000.data.tmp");
        let meta_tmp = pool_dir.join("slot-0000.meta.tmp");
        tokio::fs::write(&data_tmp, b"stale").await.unwrap();
        write_meta_tmp(&meta_tmp, "b", "k").await;

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report.half_renamed_fixed, 1);
        assert_eq!(report.recovered_pairs, 0);

        // Destination data must be untouched, meta must now be at dest.
        assert_eq!(std::fs::read(dest_dir.join("shard.dat")).unwrap(), b"final");
        assert!(dest_dir.join("meta.json").exists());
        assert!(!data_tmp.exists());
        assert!(!meta_tmp.exists());
    }

    #[tokio::test]
    async fn recover_meta_only_with_dest_present_finishes_meta() {
        // Only slot-N.meta.tmp exists (data already renamed AND the
        // stale temp data was somehow already cleaned up). If
        // data_dest exists, we're half-renamed: finish the meta.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        tokio::fs::create_dir_all(&pool_dir).await.unwrap();

        let dest_dir = root.join("b").join("k");
        tokio::fs::create_dir_all(&dest_dir).await.unwrap();
        tokio::fs::write(dest_dir.join("shard.dat"), b"final").await.unwrap();

        let meta_tmp = pool_dir.join("slot-0000.meta.tmp");
        write_meta_tmp(&meta_tmp, "b", "k").await;

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report.half_renamed_fixed, 1);
        assert!(dest_dir.join("meta.json").exists());
        assert!(!meta_tmp.exists());
    }

    #[tokio::test]
    async fn recover_meta_only_without_dest_deletes_meta() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        tokio::fs::create_dir_all(&pool_dir).await.unwrap();

        // Meta references bucket=b key=k but shard.dat doesn't exist.
        let meta_tmp = pool_dir.join("slot-0000.meta.tmp");
        write_meta_tmp(&meta_tmp, "b", "k").await;

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report.orphans_deleted, 1);
        assert!(!meta_tmp.exists());
        // Destination should not have been fabricated.
        assert!(!root.join("b").join("k").join("meta.json").exists());
    }

    #[tokio::test]
    async fn recover_data_only_deletes_data() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        tokio::fs::create_dir_all(&pool_dir).await.unwrap();

        let data_tmp = pool_dir.join("slot-0000.data.tmp");
        tokio::fs::write(&data_tmp, b"unacked").await.unwrap();

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report.orphans_deleted, 1);
        assert!(!data_tmp.exists());
    }

    #[tokio::test]
    async fn recover_unparseable_meta_deletes_both() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        tokio::fs::create_dir_all(&pool_dir).await.unwrap();

        let data_tmp = pool_dir.join("slot-0000.data.tmp");
        let meta_tmp = pool_dir.join("slot-0000.meta.tmp");
        tokio::fs::write(&data_tmp, b"bytes").await.unwrap();
        tokio::fs::write(&meta_tmp, b"not json").await.unwrap();

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report.unparseable_deleted, 1);
        assert!(!data_tmp.exists());
        assert!(!meta_tmp.exists());
    }

    #[tokio::test]
    async fn recover_empty_meta_deletes_both() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        tokio::fs::create_dir_all(&pool_dir).await.unwrap();

        let data_tmp = pool_dir.join("slot-0000.data.tmp");
        let meta_tmp = pool_dir.join("slot-0000.meta.tmp");
        tokio::fs::write(&data_tmp, b"bytes").await.unwrap();
        tokio::fs::File::create(&meta_tmp).await.unwrap(); // empty

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report.unparseable_deleted, 1);
        assert!(!data_tmp.exists());
        assert!(!meta_tmp.exists());
    }

    #[tokio::test]
    async fn recover_meta_without_bucket_or_key_deletes_both() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        tokio::fs::create_dir_all(&pool_dir).await.unwrap();

        let data_tmp = pool_dir.join("slot-0000.data.tmp");
        let meta_tmp = pool_dir.join("slot-0000.meta.tmp");
        tokio::fs::write(&data_tmp, b"bytes").await.unwrap();
        // Valid JSON but bucket/key both empty (e.g. an old-format
        // meta.json file that somehow landed in the pool dir).
        let mf = ObjectMetaFile::default();
        let json = serde_json::to_vec(&mf).unwrap();
        tokio::fs::write(&meta_tmp, json).await.unwrap();

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report.unparseable_deleted, 1);
        assert!(!data_tmp.exists());
        assert!(!meta_tmp.exists());
    }

    #[tokio::test]
    async fn recover_stray_files_are_deleted() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        tokio::fs::create_dir_all(&pool_dir).await.unwrap();

        let stray = pool_dir.join("not-a-slot.txt");
        tokio::fs::write(&stray, b"junk").await.unwrap();

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report, RecoveryReport::default());
        assert!(!stray.exists());
    }

    #[tokio::test]
    async fn recover_empty_pool_dir_is_noop() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        tokio::fs::create_dir_all(&pool_dir).await.unwrap();

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report, RecoveryReport::default());
    }

    #[tokio::test]
    async fn recover_missing_pool_dir_creates_it_and_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        // Deliberately don't create it.
        assert!(!pool_dir.exists());

        let report = recover_pool_dir(root, &pool_dir).await.unwrap();
        assert_eq!(report, RecoveryReport::default());
        assert!(pool_dir.exists());
    }

    #[test]
    fn classify_slot_filename_accepts_valid_names() {
        assert!(matches!(
            classify_slot_filename("slot-0000.data.tmp"),
            Some((0, SlotHalf::Data))
        ));
        assert!(matches!(
            classify_slot_filename("slot-0031.meta.tmp"),
            Some((31, SlotHalf::Meta))
        ));
    }

    #[test]
    fn classify_slot_filename_rejects_bad_names() {
        assert!(classify_slot_filename("slot-0000.dat.tmp").is_none());
        assert!(classify_slot_filename("slot-00.data.tmp").is_none()); // wrong digit count
        assert!(classify_slot_filename("slot-abcd.data.tmp").is_none());
        assert!(classify_slot_filename("shard.dat").is_none());
        assert!(classify_slot_filename("random.txt").is_none());
    }
}
