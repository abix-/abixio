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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use tokio::fs::File;

use super::StorageError;

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
}
