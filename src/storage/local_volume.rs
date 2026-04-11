use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;

use super::metadata::{
    BucketSettings, ObjectMeta, ObjectMetaFile, read_meta_file, write_meta_file,
};
use super::pathing;
use super::write_slot_pool::{
    recover_pool_dir, run_rename_worker, PendingEntry, PendingRenames, RenameRequest, WriteSlot,
    WriteSlotPool,
};
use super::{Backend, BackendInfo, MmapOrVec, ShardWriter, StorageError};

pub struct LocalVolume {
    root: PathBuf,
    volume_id: String,
    log_store: Option<std::sync::Mutex<super::log_store::LogStore>>,
    /// Pre-opened temp file pool for the fast write path. None until
    /// `enable_write_pool` is called. See `docs/write-pool.md`.
    write_pool: Option<Arc<WriteSlotPool>>,
    /// Sender for rename requests; held alongside the pool. Closing
    /// this drains the worker.
    rename_tx: Option<tokio::sync::mpsc::Sender<RenameRequest>>,
    /// Shutdown signal for the rename worker.
    pool_shutdown: Option<tokio::sync::watch::Sender<bool>>,
    /// In-memory table of objects that have been written to a slot
    /// but whose temp files haven't been renamed to their destination
    /// yet. Read paths consult this so an immediate GET-after-PUT
    /// finds the object before the worker finishes the rename.
    pending_renames: Option<PendingRenames>,
}

/// Default threshold: objects <= 64KB use log-structured storage.
const LOG_THRESHOLD: usize = 64 * 1024;

const META_FILE: &str = "meta.json";
const BUCKET_SETTINGS_FILE: &str = "settings.json";

impl LocalVolume {
    pub fn new(root: &Path) -> Result<Self, StorageError> {
        if !root.is_dir() {
            return Err(StorageError::InvalidConfig(format!(
                "disk path does not exist: {}",
                root.display()
            )));
        }
        // ensure tmp dir exists
        let tmp = root.join(".abixio.tmp");
        std::fs::create_dir_all(&tmp)?;
        // read volume_id from volume.json if available
        let volume_id = crate::storage::volume::read_volume_format(root)
            .map(|f| f.volume_id)
            .unwrap_or_default();
        // log-structured store for small objects
        // auto-enable when .abixio.sys/log/ already exists (e.g. server restart)
        // create with enable_log_store() or --log-store CLI flag
        let log_dir = root.join(".abixio.sys").join("log");
        let log_store = if log_dir.is_dir() {
            match super::log_store::LogStore::open(&log_dir, super::segment::DEFAULT_SEGMENT_SIZE) {
                Ok(ls) => Some(std::sync::Mutex::new(ls)),
                Err(e) => {
                    tracing::warn!("log store init failed: {}", e);
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            root: root.to_path_buf(),
            volume_id,
            log_store,
            write_pool: None,
            rename_tx: None,
            pool_shutdown: None,
            pending_renames: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Enable the log-structured store for this volume.
    /// Creates the log directory if it doesn't exist.
    pub fn enable_log_store(&mut self) -> Result<(), StorageError> {
        if self.log_store.is_some() {
            return Ok(());
        }
        let log_dir = self.root.join(".abixio.sys").join("log");
        std::fs::create_dir_all(&log_dir)?;
        let ls = super::log_store::LogStore::open(&log_dir, super::segment::DEFAULT_SEGMENT_SIZE)?;
        self.log_store = Some(std::sync::Mutex::new(ls));
        Ok(())
    }

    /// Enable the pre-opened temp file pool for this volume.
    /// Creates the pool directory at `.abixio.sys/tmp/`, opens
    /// `depth` slot pairs, and spawns the rename worker as a tokio
    /// task. The worker drains rename requests until shutdown is
    /// signaled (when `LocalVolume` is dropped or `disable_write_pool`
    /// is called).
    ///
    /// Phase 4 only wires the buffered `write_shard` path; streaming
    /// (`open_shard_writer`), reads, crash recovery, and the CLI flag
    /// land in later phases. See `docs/write-pool.md` for status.
    pub async fn enable_write_pool(&mut self, depth: u32) -> Result<(), StorageError> {
        self.enable_write_pool_with_channel(depth, 256).await
    }

    /// Same as `enable_write_pool` but lets the caller pick the
    /// rename worker's channel buffer size. Used by benches to
    /// measure what happens when channel backpressure is removed.
    /// Production should stick with the default 256.
    pub async fn enable_write_pool_with_channel(
        &mut self,
        depth: u32,
        channel_buffer: usize,
    ) -> Result<(), StorageError> {
        if self.write_pool.is_some() {
            return Ok(());
        }
        let pool_dir = self.root.join(".abixio.sys").join("tmp");

        // Phase 6: finish any pending renames left over from a crash
        // BEFORE WriteSlotPool::new truncates the slot files. Acked
        // PUTs that didn't finish their rename on the previous run
        // are recovered here. See write_slot_pool::recover_pool_dir.
        let report = recover_pool_dir(&self.root, &pool_dir).await?;
        if report.recovered_pairs
            + report.half_renamed_fixed
            + report.orphans_deleted
            + report.unparseable_deleted
            > 0
        {
            tracing::info!(
                recovered = report.recovered_pairs,
                half_renamed_fixed = report.half_renamed_fixed,
                orphans_deleted = report.orphans_deleted,
                unparseable_deleted = report.unparseable_deleted,
                pool_dir = %pool_dir.display(),
                "write pool crash recovery scan complete",
            );
        }

        let pool = WriteSlotPool::new(&pool_dir, depth).await?;
        let (tx, rx) = tokio::sync::mpsc::channel::<RenameRequest>(channel_buffer);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Phase 5: shared pending-renames table -- write_shard inserts,
        // the rename worker removes after each rename completes, the
        // read paths consult it for read-after-write consistency.
        let pending: PendingRenames = Arc::new(dashmap::DashMap::new());

        let pool_clone = Arc::clone(&pool);
        let pending_clone = Arc::clone(&pending);
        tokio::spawn(async move {
            run_rename_worker(pool_clone, Some(pending_clone), rx, shutdown_rx).await;
        });

        self.write_pool = Some(pool);
        self.rename_tx = Some(tx);
        self.pool_shutdown = Some(shutdown_tx);
        self.pending_renames = Some(pending);
        Ok(())
    }

    /// Wait until every in-flight rename has completed and every
    /// slot is back in the pool. Used by tests to bridge the gap
    /// between PUT-ack and the file being readable at its final
    /// destination. Times out after 5 seconds so a buggy test fails
    /// fast instead of hanging forever.
    pub async fn drain_pending(&self) {
        let Some(ref pool) = self.write_pool else {
            return;
        };
        let target = pool.depth() as usize;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if pool.available() >= target {
                break;
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    available = pool.available(),
                    target,
                    "drain_pending timed out -- pool slots not returning"
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// Whether the write pool is enabled.
    pub fn write_pool_enabled(&self) -> bool {
        self.write_pool.is_some()
    }

    /// Write a shard to the log store (for small objects).
    /// Returns the needle location, or None if log store not available.
    pub fn write_shard_to_log(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<Option<super::needle::NeedleLocation>, StorageError> {
        let Some(ref log_mutex) = self.log_store else {
            return Ok(None);
        };
        let needle = super::needle::Needle::new(bucket, key, meta, data)?;
        let mut log = log_mutex.lock().map_err(|e| {
            StorageError::Internal(format!("log store lock: {}", e))
        })?;
        let loc = log.append(&needle)?;
        // no fsync: trust OS page cache (same as MinIO/RustFS).
        // mmap reads see the write immediately via shared page cache.
        Ok(Some(loc))
    }

    /// Read shard data from the log store.
    /// Returns (data_slice, ObjectMeta) or None if not in log.
    pub fn read_shard_from_log(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(Vec<u8>, ObjectMeta)>, StorageError> {
        let Some(ref log_mutex) = self.log_store else {
            return Ok(None);
        };
        let log = log_mutex.lock().map_err(|e| {
            StorageError::Internal(format!("log store lock: {}", e))
        })?;
        let Some(loc) = log.get(bucket, key) else {
            return Ok(None);
        };
        let loc = *loc;
        let data = log.read_data_vec(&loc)
            .ok_or(StorageError::ObjectNotFound)?;
        let meta = log.read_meta(&loc)?;
        Ok(Some((data, meta)))
    }

    /// Check if an object exists in the log store.
    pub fn is_in_log(&self, bucket: &str, key: &str) -> bool {
        self.log_store
            .as_ref()
            .and_then(|m| m.lock().ok())
            .map_or(false, |log| log.contains(bucket, key))
    }

    /// Whether the log store is available and the original object size is below threshold.
    pub fn should_use_log(&self, original_object_size: u64) -> bool {
        self.log_store.is_some() && (original_object_size as usize) <= LOG_THRESHOLD
    }

    async fn walk_keys(
        base: &Path,
        dir: &Path,
        prefix: &str,
        keys: &mut Vec<String>,
    ) -> Result<(), StorageError> {
        pathing::validate_object_prefix(prefix)?;
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                // check if this dir is an object (has meta.json)
                if entry.path().join(META_FILE).exists() {
                    let rel = entry
                        .path()
                        .strip_prefix(base)
                        .map_err(|_| StorageError::Internal(
                            "object path not under base directory".into(),
                        ))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    if rel.starts_with(prefix) {
                        keys.push(rel);
                    }
                } else {
                    // recurse into subdirectory
                    Box::pin(Self::walk_keys(base, &entry.path(), prefix, keys)).await?;
                }
            }
        }
        Ok(())
    }
}

impl LocalVolume {
    /// Write a part shard file (part.N) into the object directory.
    pub async fn write_part_shard(
        &self,
        bucket: &str,
        key: &str,
        part_number: i32,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        tokio::fs::create_dir_all(&obj_dir).await?;
        let part_path = obj_dir.join(format!("part.{}", part_number));
        tokio::fs::write(part_path, data).await?;
        Ok(())
    }

    /// Read a part shard file (part.N) from the object directory.
    pub async fn read_part_shard(
        &self,
        bucket: &str,
        key: &str,
        part_number: i32,
    ) -> Result<Vec<u8>, StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        let part_path = obj_dir.join(format!("part.{}", part_number));
        tokio::fs::read(&part_path).await.map_err(|_| StorageError::ObjectNotFound)
    }

    /// Write only meta.json without shard data (used by multipart complete).
    pub async fn write_meta_only(
        &self,
        bucket: &str,
        key: &str,
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        tokio::fs::create_dir_all(&obj_dir).await?;
        let mut version = meta.clone();
        version.is_latest = true;
        let mf = ObjectMetaFile {
            bucket: bucket.to_string(),
            key: key.to_string(),
            versions: vec![version],
        };
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await
            .map_err(StorageError::Io)
    }
}

/// Streaming shard writer for local disk. Opens a file on creation,
/// appends chunks, writes meta on finalize.
pub struct LocalShardWriter {
    file: tokio::fs::File,
    root: PathBuf,
    bucket: String,
    key: String,
    version_id: Option<String>,
}

#[async_trait::async_trait]
impl ShardWriter for LocalShardWriter {
    async fn write_chunk(&mut self, data: &[u8]) -> Result<(), StorageError> {
        self.file.write_all(data).await?;
        Ok(())
    }

    async fn finalize(mut self: Box<Self>, meta: &ObjectMeta) -> Result<(), StorageError> {
        self.file.flush().await?;
        drop(self.file);

        if let Some(ref vid) = self.version_id {
            // versioned: update meta.json adding new version at front
            let meta_path = pathing::object_meta_path(&self.root, &self.bucket, &self.key)?;
            let mut mf = read_meta_file(&meta_path).await.unwrap_or_else(|_| ObjectMetaFile {
                bucket: self.bucket.clone(),
                key: self.key.clone(),
                versions: Vec::new(),
            });
            // Ensure identity fields are populated even if read from an
            // older meta.json that lacked them.
            if mf.bucket.is_empty() {
                mf.bucket = self.bucket.clone();
            }
            if mf.key.is_empty() {
                mf.key = self.key.clone();
            }
            for v in &mut mf.versions {
                v.is_latest = false;
            }
            let mut version = meta.clone();
            version.is_latest = true;
            version.version_id = vid.clone();
            mf.versions.insert(0, version);
            write_meta_file(&meta_path, &mf).await.map_err(StorageError::Io)?;
        } else {
            // unversioned: single version entry
            let mut version = meta.clone();
            version.is_latest = true;
            let mf = ObjectMetaFile {
                bucket: self.bucket.clone(),
                key: self.key.clone(),
                versions: vec![version],
            };
            let meta_path = pathing::object_meta_path(&self.root, &self.bucket, &self.key)?;
            write_meta_file(&meta_path, &mf).await.map_err(StorageError::Io)?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Backend for LocalVolume {
    async fn drain_pending_writes(&self) {
        self.drain_pending().await;
    }

    async fn open_shard_writer(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<Box<dyn ShardWriter>, StorageError> {
        let shard_path = if let Some(vid) = version_id {
            let ver_dir = pathing::version_dir(&self.root, bucket, key, vid)?;
            tokio::fs::create_dir_all(&ver_dir).await?;
            pathing::version_shard_path(&self.root, bucket, key, vid)?
        } else {
            let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
            tokio::fs::create_dir_all(&obj_dir).await?;
            pathing::object_shard_path(&self.root, bucket, key)?
        };
        let file = tokio::fs::File::create(&shard_path).await?;
        Ok(Box::new(LocalShardWriter {
            file,
            root: self.root.clone(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            version_id: version_id.map(|s| s.to_string()),
        }))
    }

    async fn write_shard(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        // validate key (same checks for both paths)
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;

        let is_versioned = !meta.version_id.is_empty();

        // Pool fast path: if the write pool is enabled and a slot is
        // available, use the Phase 2 hot path (pop slot, concurrent
        // writes via try_join, simd-json meta, send rename request).
        // Skip for versioned objects -- versioned needs a meta.json
        // read-modify-write that the pool's "pure rename" worker
        // can't do yet (Phase 5+).
        if !is_versioned {
            if let (Some(pool), Some(tx)) = (self.write_pool.as_ref(), self.rename_tx.as_ref()) {
                if let Some(slot) = pool.try_pop() {
                    let mut version = meta.clone();
                    version.is_latest = true;
                    let mf = ObjectMetaFile {
                        bucket: bucket.to_string(),
                        key: key.to_string(),
                        versions: vec![version],
                    };
                    // simd-json was the Phase 2.5 winner: 30% faster
                    // than serde_json::to_vec on a ~500-byte struct
                    let meta_json = simd_json::serde::to_vec(&mf).map_err(|e| {
                        StorageError::Internal(format!("simd_json meta serialize: {}", e))
                    })?;

                    // Phase 5.6: don't destructure the slot. Write through
                    // its file handles via split borrows, then move the
                    // whole slot into the rename request so the worker
                    // (not the request handler) drops the file handles.
                    // The drops are a major hot-path cost on Windows
                    // (~65us avg, ~1.57ms p99 from kernel close);
                    // moving them to the worker keeps the hot path tight.
                    let mut slot = slot;
                    tokio::try_join!(
                        slot.data_file.write_all(data),
                        slot.meta_file.write_all(&meta_json),
                    )?;

                    // Compute destination paths once. (Phase 4.5: collapsing
                    // 3 path computations into 1 saved ~5us / call.)
                    let dest_dir = pathing::object_dir(&self.root, bucket, key)?;
                    let data_dest = dest_dir.join("shard.dat");
                    let meta_dest = dest_dir.join("meta.json");

                    // Phase 5: insert into pending_renames BEFORE sending
                    // the rename request so concurrent readers see the
                    // entry as soon as the writes are durable. The insert
                    // must happen before the channel send to avoid the
                    // race where the worker starts processing before the
                    // entry exists. Phase 5.5: slim PendingEntry; the
                    // read paths read the temp meta file when they need
                    // it instead of caching the ObjectMeta in RAM.
                    if let Some(ref pending) = self.pending_renames {
                        let entry = PendingEntry {
                            slot_id: slot.slot_id,
                            data_path: slot.data_path.clone(),
                            meta_path: slot.meta_path.clone(),
                            data_len: data.len() as u64,
                        };
                        pending.insert(
                            (Arc::<str>::from(bucket), Arc::<str>::from(key)),
                            entry,
                        );
                    }

                    let req = RenameRequest {
                        slot,
                        bucket: bucket.to_string(),
                        key: key.to_string(),
                        dest_dir,
                        data_dest,
                        meta_dest,
                    };
                    tx.send(req).await.map_err(|_| {
                        StorageError::Internal("rename worker channel closed".into())
                    })?;
                    return Ok(());
                }
                // Pool empty: fall through to the existing slow path
                // (Phase 7 will add explicit backpressure / fallback).
            }
        }

        // small objects: log-structured path (one append instead of mkdir + 2 file creates)
        // skip for versioned objects (log store doesn't support version chains yet)
        if !is_versioned && self.should_use_log(meta.size) {
            self.write_shard_to_log(bucket, key, data, meta)?;
            return Ok(());
        }

        // file path
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        tokio::fs::create_dir_all(&obj_dir).await?;

        let mut version = meta.clone();
        version.is_latest = true;

        // small objects: inline shard data in meta.json (one file instead of two)
        if !is_versioned && (meta.size as usize) <= LOG_THRESHOLD {
            use base64::Engine;
            version.inline_data = Some(
                base64::engine::general_purpose::STANDARD.encode(data)
            );
            // no shard.dat -- data is in meta.json
        } else {
            // large objects: separate shard.dat
            tokio::fs::write(pathing::object_shard_path(&self.root, bucket, key)?, data).await?;
        }

        let mf = ObjectMetaFile {
            bucket: bucket.to_string(),
            key: key.to_string(),
            versions: vec![version],
        };
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await
            .map_err(StorageError::Io)?;

        Ok(())
    }

    async fn read_shard(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        // Phase 5: check pending pool writes first. The temp files are
        // on disk via the page cache. Phase 5.5: we read the meta from
        // the temp meta file rather than caching it in RAM, since the
        // file was just written and is essentially free to read back.
        //
        // CRITICAL: clone the paths we need and drop the DashMap guard
        // BEFORE awaiting. Holding a DashMap Ref across an .await can
        // deadlock the worker, which needs to write-lock the same
        // shard to remove the entry after a successful rename.
        if let Some(ref pending) = self.pending_renames {
            let key_arc = (Arc::<str>::from(bucket), Arc::<str>::from(key));
            let lookup = pending
                .get(&key_arc)
                .map(|e| (e.data_path.clone(), e.meta_path.clone()));
            if let Some((data_path, meta_path)) = lookup {
                if let Ok(data) = tokio::fs::read(&data_path).await {
                    if let Ok(mf) = read_meta_file(&meta_path).await {
                        if let Some(version) = mf.versions.into_iter().next() {
                            return Ok((data, version));
                        }
                    }
                }
                // temp file(s) gone -> fall through (race with worker)
            }
        }

        // check log store first (small objects)
        if let Ok(Some((data, meta))) = self.read_shard_from_log(bucket, key) {
            return Ok((data, meta));
        }

        // fall through to file tier
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await
            .map_err(|_| StorageError::ObjectNotFound)?;

        // find latest non-delete-marker version
        let version = mf
            .versions
            .iter()
            .find(|v| !v.is_delete_marker)
            .ok_or(StorageError::ObjectNotFound)?;

        // check for inline data first (small objects have shard data in meta.json)
        if let Some(ref inline_b64) = version.inline_data {
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD
                .decode(inline_b64)
                .map_err(|e| StorageError::Internal(format!("inline base64 decode: {}", e)))?;
            let mut meta = version.clone();
            meta.inline_data = None; // don't expose internal detail to callers
            return Ok((data, meta));
        }

        // large objects: read from shard.dat
        let shard_path = if version.version_id.is_empty() {
            pathing::object_shard_path(&self.root, bucket, key)?
        } else {
            pathing::version_shard_path(&self.root, bucket, key, &version.version_id)?
        };

        let data = tokio::fs::read(&shard_path).await.map_err(|_| StorageError::ObjectNotFound)?;
        Ok((data, version.clone()))
    }

    async fn read_shard_stream(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send + Unpin>>, ObjectMeta), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await
            .map_err(|_| StorageError::ObjectNotFound)?;
        let version = mf
            .versions
            .iter()
            .find(|v| !v.is_delete_marker)
            .ok_or(StorageError::ObjectNotFound)?;

        // inline data: return cursor over decoded bytes
        if let Some(ref inline_b64) = version.inline_data {
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD
                .decode(inline_b64)
                .map_err(|e| StorageError::Internal(format!("inline base64: {}", e)))?;
            let mut meta = version.clone();
            meta.inline_data = None;
            return Ok((Box::pin(std::io::Cursor::new(data)), meta));
        }

        let shard_path = if version.version_id.is_empty() {
            pathing::object_shard_path(&self.root, bucket, key)?
        } else {
            pathing::version_shard_path(&self.root, bucket, key, &version.version_id)?
        };
        let file = tokio::fs::File::open(&shard_path).await.map_err(|_| StorageError::ObjectNotFound)?;
        Ok((Box::pin(file), version.clone()))
    }

    async fn mmap_shard(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(MmapOrVec, super::metadata::ObjectMeta), StorageError> {
        // Phase 5: check pending pool writes first (same race fallback
        // as read_shard). Phase 5.5: read meta from the temp meta file.
        if let Some(ref pending) = self.pending_renames {
            let key_arc = (Arc::<str>::from(bucket), Arc::<str>::from(key));
            let lookup = pending
                .get(&key_arc)
                .map(|e| (e.data_path.clone(), e.meta_path.clone()));
            if let Some((data_path, meta_path)) = lookup {
                if let Ok(file) = std::fs::File::open(&data_path) {
                    if let Ok(mmap) = unsafe { memmap2::Mmap::map(&file) } {
                        if let Ok(mf) = read_meta_file(&meta_path).await {
                            if let Some(version) = mf.versions.into_iter().next() {
                                return Ok((MmapOrVec::Mmap(mmap), version));
                            }
                        }
                    }
                }
                // race with worker -> fall through to file tier
            }
        }

        // check log store first (small objects)
        if let Ok(Some((data, meta))) = self.read_shard_from_log(bucket, key) {
            return Ok((MmapOrVec::Vec(data), meta));
        }

        // file tier
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await
            .map_err(|_| StorageError::ObjectNotFound)?;
        let version = mf
            .versions
            .iter()
            .find(|v| !v.is_delete_marker)
            .ok_or(StorageError::ObjectNotFound)?;

        // inline data: return from meta.json (no shard.dat)
        if let Some(ref inline_b64) = version.inline_data {
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD
                .decode(inline_b64)
                .map_err(|e| StorageError::Internal(format!("inline base64: {}", e)))?;
            let mut meta = version.clone();
            meta.inline_data = None;
            return Ok((MmapOrVec::Vec(data), meta));
        }

        let shard_path = if version.version_id.is_empty() {
            pathing::object_shard_path(&self.root, bucket, key)?
        } else {
            pathing::version_shard_path(&self.root, bucket, key, &version.version_id)?
        };
        let file = std::fs::File::open(&shard_path).map_err(|_| StorageError::ObjectNotFound)?;
        let mmap = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        Ok((MmapOrVec::Mmap(mmap), version.clone()))
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        // Phase 5: cancel a pending pool write if one exists. Remove
        // the in-memory entry, then unlink the temp files. The worker's
        // rename will fail with ENOENT and the slot will still be
        // replenished by the always-replenish guarantee in
        // process_rename_request. We then return success without
        // touching the file tier.
        if let Some(ref pending) = self.pending_renames {
            let key_arc = (Arc::<str>::from(bucket), Arc::<str>::from(key));
            if let Some((_, entry)) = pending.remove(&key_arc) {
                let _ = tokio::fs::remove_file(&entry.data_path).await;
                let _ = tokio::fs::remove_file(&entry.meta_path).await;
                return Ok(());
            }
        }

        // check log store first
        if self.is_in_log(bucket, key) {
            if let Some(ref log_mutex) = self.log_store {
                if let Ok(mut log) = log_mutex.lock() {
                    log.delete(bucket, key)?;
                    return Ok(());
                }
            }
        }

        // file tier
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        tokio::fs::remove_dir_all(&obj_dir).await?;
        Ok(())
    }

    async fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, StorageError> {
        let bucket_dir = pathing::bucket_dir(&self.root, bucket)?;
        let mut keys = Vec::new();

        // file tier
        if bucket_dir.is_dir() {
            Self::walk_keys(&bucket_dir, &bucket_dir, prefix, &mut keys).await?;
        }

        // log store
        if let Some(ref log_mutex) = self.log_store {
            if let Ok(log) = log_mutex.lock() {
                for k in log.list_keys(bucket, prefix) {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
            }
        }

        // Phase 5: pending pool writes that haven't been renamed yet
        if let Some(ref pending) = self.pending_renames {
            for entry in pending.iter() {
                let (b, k) = entry.key();
                if b.as_ref() == bucket && k.starts_with(prefix) {
                    let k_str = k.to_string();
                    if !keys.contains(&k_str) {
                        keys.push(k_str);
                    }
                }
            }
        }

        keys.sort();
        Ok(keys)
    }

    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        let mut buckets = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if entry.file_type().await?.is_dir() {
                buckets.push(name);
            }
        }
        buckets.sort();
        Ok(buckets)
    }

    async fn make_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let path = pathing::bucket_dir(&self.root, bucket)?;
        if path.is_dir() {
            return Err(StorageError::BucketExists);
        }
        tokio::fs::create_dir_all(&path).await?;
        Ok(())
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let path = pathing::bucket_dir(&self.root, bucket)?;
        if !path.is_dir() {
            return Err(StorageError::BucketNotFound);
        }
        tokio::fs::remove_dir(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                || e.to_string().contains("not empty")
            {
                StorageError::BucketNotEmpty
            } else {
                StorageError::Io(e)
            }
        })
    }

    fn bucket_exists(&self, bucket: &str) -> bool {
        pathing::bucket_dir(&self.root, bucket)
            .map(|path| path.is_dir())
            .unwrap_or(false)
    }

    fn bucket_created_at(&self, bucket: &str) -> u64 {
        let Ok(path) = pathing::bucket_dir(&self.root, bucket) else {
            return 0;
        };
        std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    async fn stat_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StorageError> {
        // Phase 5: pending pool writes are visible to stat. Phase 5.5
        // reads the meta from the temp meta file (which is in the
        // page cache) instead of caching it in RAM at PUT time.
        if let Some(ref pending) = self.pending_renames {
            let key_arc = (Arc::<str>::from(bucket), Arc::<str>::from(key));
            let meta_path = pending.get(&key_arc).map(|e| e.meta_path.clone());
            if let Some(meta_path) = meta_path {
                if let Ok(mf) = read_meta_file(&meta_path).await {
                    if let Some(version) = mf.versions.into_iter().next() {
                        return Ok(version);
                    }
                }
                // race with worker -> fall through
            }
        }

        // check log store first
        if let Some(ref log_mutex) = self.log_store {
            if let Ok(log) = log_mutex.lock() {
                if let Some(loc) = log.get(bucket, key) {
                    if let Ok(meta) = log.read_meta(loc) {
                        return Ok(meta);
                    }
                }
            }
        }

        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await
            .map_err(|_| StorageError::ObjectNotFound)?;
        let mut meta = mf.versions
            .iter()
            .find(|v| !v.is_delete_marker)
            .cloned()
            .ok_or(StorageError::ObjectNotFound)?;
        meta.inline_data = None; // don't expose internal detail
        Ok(meta)
    }

    async fn update_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        if !obj_dir.is_dir() {
            return Err(StorageError::ObjectNotFound);
        }
        // replace the matching version entry (by index) in meta.json
        let mut mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await
            .map_err(|_| StorageError::ObjectNotFound)?;
        if let Some(v) = mf
            .versions
            .iter_mut()
            .find(|v| v.erasure.index == meta.erasure.index || v.version_id == meta.version_id)
        {
            *v = meta.clone();
        }
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await
            .map_err(StorageError::Io)
    }

    async fn write_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let ver_dir = pathing::version_dir(&self.root, bucket, key, version_id)?;
        tokio::fs::create_dir_all(&ver_dir).await?;

        // write shard data to key/<uuid>/shard.dat
        tokio::fs::write(pathing::version_shard_path(&self.root, bucket, key, version_id)?, data).await?;

        // update meta.json: add new version entry at front, mark others as not latest
        let mut mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?)
            .await
            .unwrap_or_else(|_| ObjectMetaFile {
                bucket: bucket.to_string(),
                key: key.to_string(),
                versions: Vec::new(),
            });
        if mf.bucket.is_empty() {
            mf.bucket = bucket.to_string();
        }
        if mf.key.is_empty() {
            mf.key = key.to_string();
        }
        for v in &mut mf.versions {
            v.is_latest = false;
        }
        let mut version = meta.clone();
        version.is_latest = true;
        version.version_id = version_id.to_string();
        mf.versions.insert(0, version);
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await
            .map_err(StorageError::Io)?;

        Ok(())
    }

    async fn read_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        let mf = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await
            .map_err(|_| StorageError::ObjectNotFound)?;

        let version = mf
            .versions
            .iter()
            .find(|v| v.version_id == version_id)
            .ok_or(StorageError::ObjectNotFound)?;

        let shard_path = pathing::version_shard_path(&self.root, bucket, key, version_id)?;
        let data = tokio::fs::read(&shard_path).await.map_err(|_| StorageError::ObjectNotFound)?;
        Ok((data, version.clone()))
    }

    async fn delete_version_data(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError> {
        // remove shard data dir
        let ver_dir = pathing::version_dir(&self.root, bucket, key, version_id)?;
        if ver_dir.is_dir() {
            tokio::fs::remove_dir_all(&ver_dir).await?;
        }

        // remove version entry from meta.json
        if let Ok(mut mf) = read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await {
            mf.versions.retain(|v| v.version_id != version_id);
            // update is_latest
            if let Some(first) = mf.versions.first_mut() {
                first.is_latest = true;
            }
            let _ = write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await;
        }
        Ok(())
    }

    async fn read_meta_versions(&self, bucket: &str, key: &str) -> Result<Vec<ObjectMeta>, StorageError> {
        match read_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?).await {
            Ok(mf) => Ok(mf.versions),
            Err(_) => Ok(Vec::new()),
        }
    }

    async fn write_meta_versions(
        &self,
        bucket: &str,
        key: &str,
        versions: &[ObjectMeta],
    ) -> Result<(), StorageError> {
        let obj_dir = pathing::object_dir(&self.root, bucket, key)?;
        tokio::fs::create_dir_all(&obj_dir).await?;
        let mf = ObjectMetaFile {
            bucket: bucket.to_string(),
            key: key.to_string(),
            versions: versions.to_vec(),
        };
        write_meta_file(&pathing::object_meta_path(&self.root, bucket, key)?, &mf).await
            .map_err(StorageError::Io)
    }

    async fn read_bucket_settings(&self, bucket: &str) -> BucketSettings {
        let Ok(path) = pathing::bucket_settings_path(&self.root, bucket) else {
            return BucketSettings::default();
        };
        tokio::fs::read(&path).await
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    async fn write_bucket_settings(
        &self,
        bucket: &str,
        settings: &BucketSettings,
    ) -> Result<(), StorageError> {
        let dir = pathing::bucket_settings_dir(&self.root, bucket)?;
        tokio::fs::create_dir_all(&dir).await?;
        let data = serde_json::to_vec_pretty(settings)
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        tokio::fs::write(dir.join(BUCKET_SETTINGS_FILE), data).await?;
        Ok(())
    }

    fn info(&self) -> BackendInfo {
        let (total_bytes, free_bytes) = disk_space(&self.root);
        let used_bytes = total_bytes.saturating_sub(free_bytes);
        BackendInfo {
            label: format!("local:{}", self.root.display()),
            volume_id: self.volume_id.clone(),
            backend_type: "local".to_string(),
            total_bytes: Some(total_bytes),
            used_bytes: Some(used_bytes),
            free_bytes: Some(free_bytes),
        }
    }

    fn set_volume_id(&mut self, id: String) {
        self.volume_id = id;
    }
}

fn disk_space(path: &Path) -> (u64, u64) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free_available: u64 = 0;
        let mut total: u64 = 0;
        let mut _total_free: u64 = 0;
        unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_available as *mut u64 as *mut _,
                &mut total as *mut u64 as *mut _,
                &mut _total_free as *mut u64 as *mut _,
            );
        }
        (total, free_available)
    }

    #[cfg(not(target_os = "windows"))]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_path = match CString::new(path.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(_) => return (0, 0),
        };
        unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                let total = stat.f_blocks as u64 * stat.f_frsize as u64;
                let free = stat.f_bavail as u64 * stat.f_frsize as u64;
                (total, free)
            } else {
                (0, 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::metadata::ErasureMeta;
    use tempfile::TempDir;

    fn test_meta(index: usize) -> ObjectMeta {
        ObjectMeta {
            size: 100,
            etag: "abc".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                ftt: 2,
                index,
                epoch_id: 1,
                volume_ids: vec![
                    "vol-a".to_string(),
                    "vol-b".to_string(),
                    "vol-c".to_string(),
                    "vol-d".to_string(),
                ],
            },
            checksum: "deadbeef".to_string(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
                inline_data: None,
        }
    }

    #[tokio::test]
    async fn new_valid_dir() {
        let dir = TempDir::new().unwrap();
        assert!(LocalVolume::new(dir.path()).is_ok());
    }

    #[tokio::test]
    async fn new_missing_dir() {
        let result = LocalVolume::new(Path::new("/nonexistent"));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn bucket_lifecycle() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        assert!(!disk.bucket_exists("test"));
        disk.make_bucket("test").await.unwrap();
        assert!(disk.bucket_exists("test"));
    }

    #[tokio::test]
    async fn make_bucket_already_exists() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        assert!(matches!(
            disk.make_bucket("test").await,
            Err(StorageError::BucketExists)
        ));
    }

    #[tokio::test]
    async fn bucket_exists_missing() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        assert!(!disk.bucket_exists("nope"));
    }

    #[tokio::test]
    async fn write_read_shard() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        let data = b"hello world";
        disk.write_shard("test", "mykey", data, &meta).await.unwrap();
        let (read_data, read_meta) = disk.read_shard("test", "mykey").await.unwrap();
        assert_eq!(read_data, data);
        assert_eq!(read_meta, meta);
    }

    #[tokio::test]
    async fn write_shard_nested_key() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "a/b/c", b"nested", &meta).await.unwrap();
        let (data, _) = disk.read_shard("test", "a/b/c").await.unwrap();
        assert_eq!(data, b"nested");
    }

    #[tokio::test]
    async fn write_shard_rejects_hostile_key() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);

        let err = disk.write_shard("test", "a/../b", b"bad", &meta).await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidObjectKey(_)));
    }

    #[tokio::test]
    async fn write_versioned_shard_rejects_hostile_version_id() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);

        let err = disk
            .write_versioned_shard("test", "safe", "../escape", b"bad", &meta)
            .await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidVersionId(_)));
    }

    #[tokio::test]
    async fn read_shard_missing_returns_error() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        assert!(matches!(
            disk.read_shard("test", "nope").await,
            Err(StorageError::ObjectNotFound)
        ));
    }

    #[tokio::test]
    async fn delete_object_removes_dir() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "key", b"data", &meta).await.unwrap();
        disk.delete_object("test", "key").await.unwrap();
        assert!(matches!(
            disk.read_shard("test", "key").await,
            Err(StorageError::ObjectNotFound)
        ));
    }

    #[tokio::test]
    async fn list_objects_returns_written_keys() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "aaa", b"1", &meta).await.unwrap();
        disk.write_shard("test", "bbb", b"2", &meta).await.unwrap();
        let keys = disk.list_objects("test", "").await.unwrap();
        assert_eq!(keys, vec!["aaa", "bbb"]);
    }

    #[tokio::test]
    async fn list_objects_with_prefix() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "logs/a", b"1", &meta).await.unwrap();
        disk.write_shard("test", "logs/b", b"2", &meta).await.unwrap();
        disk.write_shard("test", "data/c", b"3", &meta).await.unwrap();
        let keys = disk.list_objects("test", "logs/").await.unwrap();
        assert_eq!(keys, vec!["logs/a", "logs/b"]);
    }

    #[tokio::test]
    async fn stat_object_returns_meta() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        disk.make_bucket("test").await.unwrap();
        let meta = test_meta(0);
        disk.write_shard("test", "key", b"data", &meta).await.unwrap();
        let loaded = disk.stat_object("test", "key").await.unwrap();
        assert_eq!(loaded, meta);
    }

    #[tokio::test]
    async fn info_returns_local_backend() {
        let dir = TempDir::new().unwrap();
        let disk = LocalVolume::new(dir.path()).unwrap();
        let info = disk.info();
        assert_eq!(info.backend_type, "local");
        assert!(info.label.starts_with("local:"));
    }

    #[tokio::test]
    async fn log_store_small_object_round_trip() {
        let dir = TempDir::new().unwrap();
        let mut disk = LocalVolume::new(dir.path()).unwrap();
        disk.enable_log_store().unwrap();
        disk.make_bucket("test").await.unwrap();

        let meta = ObjectMeta {
            size: 11,
            etag: "abc".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                ftt: 0, index: 0, epoch_id: 1,
                volume_ids: vec!["v1".to_string()],
            },
            checksum: "dead".to_string(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
                inline_data: None,
        };

        // write via log store (small object)
        disk.write_shard("test", "hello.txt", b"hello world", &meta).await.unwrap();

        // should be in log, not in file tier
        assert!(disk.is_in_log("test", "hello.txt"));
        let shard_path = pathing::object_shard_path(dir.path(), "test", "hello.txt").unwrap();
        assert!(!shard_path.exists(), "small object should NOT be in shard.dat");

        // read back
        let (data, got_meta) = disk.read_shard("test", "hello.txt").await.unwrap();
        assert_eq!(data, b"hello world");
        assert_eq!(got_meta.etag, "abc");

        // stat
        let stat = disk.stat_object("test", "hello.txt").await.unwrap();
        assert_eq!(stat.size, 11);

        // delete
        disk.delete_object("test", "hello.txt").await.unwrap();
        assert!(!disk.is_in_log("test", "hello.txt"));
        assert!(disk.read_shard("test", "hello.txt").await.is_err());
    }

    #[tokio::test]
    async fn log_store_large_object_bypasses_log() {
        let dir = TempDir::new().unwrap();
        let mut disk = LocalVolume::new(dir.path()).unwrap();
        disk.enable_log_store().unwrap();
        disk.make_bucket("test").await.unwrap();

        let meta = ObjectMeta {
            size: 100_000,
            etag: "big".to_string(),
            content_type: "application/octet-stream".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                ftt: 0, index: 0, epoch_id: 1,
                volume_ids: vec!["v1".to_string()],
            },
            checksum: "beef".to_string(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
                inline_data: None,
        };

        let big_data = vec![0x42u8; 100_000]; // 100KB > 64KB threshold
        disk.write_shard("test", "big.bin", &big_data, &meta).await.unwrap();

        // should NOT be in log (too large)
        assert!(!disk.is_in_log("test", "big.bin"));
        // should be in file tier
        let shard_path = pathing::object_shard_path(dir.path(), "test", "big.bin").unwrap();
        assert!(shard_path.exists());
    }

    #[tokio::test]
    async fn log_store_list_includes_log_objects() {
        let dir = TempDir::new().unwrap();
        let mut disk = LocalVolume::new(dir.path()).unwrap();
        disk.enable_log_store().unwrap();
        disk.make_bucket("test").await.unwrap();

        let meta = ObjectMeta {
            size: 5,
            etag: "e".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                ftt: 0, index: 0, epoch_id: 1,
                volume_ids: vec!["v1".to_string()],
            },
            checksum: "c".to_string(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
                inline_data: None,
        };

        disk.write_shard("test", "a.txt", b"aaaaa", &meta).await.unwrap();
        disk.write_shard("test", "b.txt", b"bbbbb", &meta).await.unwrap();

        let keys = disk.list_objects("test", "").await.unwrap();
        assert!(keys.contains(&"a.txt".to_string()));
        assert!(keys.contains(&"b.txt".to_string()));
    }

    // ----- Phase 4: write pool integration tests -----

    #[tokio::test]
    async fn enable_write_pool_recovers_crashed_put() {
        // Simulate a crash state: a slot-0000.* pair exists in the
        // pool dir from a previous run, but no pool had a chance to
        // drain the rename. enable_write_pool must recover the PUT
        // BEFORE WriteSlotPool::new truncates the slot-0000.*.tmp
        // files, otherwise we would lose the acked write.

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pool_dir = root.join(".abixio.sys").join("tmp");
        std::fs::create_dir_all(&pool_dir).unwrap();

        // Lay out the temp pair by hand: data.tmp with some bytes,
        // meta.tmp with a realistic ObjectMetaFile carrying the
        // destination identity fields (bucket, key).
        let data_tmp = pool_dir.join("slot-0000.data.tmp");
        let meta_tmp = pool_dir.join("slot-0000.meta.tmp");
        std::fs::write(&data_tmp, b"recovered bytes").unwrap();
        let mf = ObjectMetaFile {
            bucket: "bucket".to_string(),
            key: "obj".to_string(),
            versions: vec![ObjectMeta {
                size: 15,
                etag: "deadbeef".to_string(),
                content_type: "application/octet-stream".to_string(),
                created_at: 0,
                erasure: ErasureMeta::default(),
                checksum: String::new(),
                ..Default::default()
            }],
        };
        std::fs::write(&meta_tmp, serde_json::to_vec(&mf).unwrap()).unwrap();

        let mut disk = LocalVolume::new(root).unwrap();
        disk.enable_write_pool(4).await.unwrap();

        // The destination must now exist with the recovered bytes.
        let shard_path = pathing::object_shard_path(root, "bucket", "obj").unwrap();
        let meta_path = pathing::object_meta_path(root, "bucket", "obj").unwrap();
        assert!(shard_path.exists(), "shard.dat should exist after recovery");
        assert!(meta_path.exists(), "meta.json should exist after recovery");
        assert_eq!(std::fs::read(&shard_path).unwrap(), b"recovered bytes");

        // Fresh pool must still be at full depth: recovery + slot
        // creation at slot ids 0..4 coexist because recovery moved
        // slot-0000.*.tmp to the destination first, then create_slot
        // reopened empty files at those paths.
        assert!(disk.write_pool_enabled());
        let fresh_data_tmp = pool_dir.join("slot-0000.data.tmp");
        let fresh_meta_tmp = pool_dir.join("slot-0000.meta.tmp");
        assert!(fresh_data_tmp.exists());
        assert!(fresh_meta_tmp.exists());
        assert_eq!(std::fs::metadata(&fresh_data_tmp).unwrap().len(), 0);
        assert_eq!(std::fs::metadata(&fresh_meta_tmp).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn enable_write_pool_creates_pool_and_worker() {
        let tmp = TempDir::new().unwrap();
        let mut disk = LocalVolume::new(tmp.path()).unwrap();
        assert!(!disk.write_pool_enabled());
        disk.enable_write_pool(8).await.unwrap();
        assert!(disk.write_pool_enabled());

        // verify the slot files exist on disk
        let pool_dir = tmp.path().join(".abixio.sys").join("tmp");
        let mut count = 0;
        for entry in std::fs::read_dir(&pool_dir).unwrap() {
            if entry.unwrap().path().is_file() {
                count += 1;
            }
        }
        assert_eq!(count, 16, "expected 8 data + 8 meta files in pool dir");
    }

    #[tokio::test]
    async fn write_shard_with_pool_routes_to_pool_then_drains() {
        let tmp = TempDir::new().unwrap();
        let mut disk = LocalVolume::new(tmp.path()).unwrap();
        disk.enable_write_pool(4).await.unwrap();

        let meta = test_meta(0);
        let payload = b"hello pool";
        disk.write_shard("bucket", "obj", payload, &meta).await.unwrap();

        // Drain pending renames so the destination file exists.
        disk.drain_pending().await;

        // The destination must now have shard.dat + meta.json
        let shard_path = pathing::object_shard_path(&tmp.path(), "bucket", "obj").unwrap();
        let meta_path = pathing::object_meta_path(&tmp.path(), "bucket", "obj").unwrap();
        assert!(shard_path.exists(), "shard.dat should exist after drain");
        assert!(meta_path.exists(), "meta.json should exist after drain");

        let read_data = std::fs::read(&shard_path).unwrap();
        assert_eq!(read_data, payload);
    }

    #[tokio::test]
    async fn read_after_drain_returns_correct_data() {
        let tmp = TempDir::new().unwrap();
        let mut disk = LocalVolume::new(tmp.path()).unwrap();
        disk.enable_write_pool(4).await.unwrap();

        let meta = test_meta(0);
        disk.write_shard("bucket", "key.txt", b"phase4data", &meta)
            .await
            .unwrap();
        disk.drain_pending().await;

        // Use the existing read_shard path which goes through the file
        // tier (Phase 4 doesn't touch reads). Verify it sees the data.
        let (got, got_meta) = disk.read_shard("bucket", "key.txt").await.unwrap();
        assert_eq!(got, b"phase4data");
        assert_eq!(got_meta.size, meta.size);
    }

    #[tokio::test]
    async fn drain_pending_is_noop_when_pool_not_enabled() {
        let tmp = TempDir::new().unwrap();
        let disk = LocalVolume::new(tmp.path()).unwrap();
        // Should return immediately without panic.
        disk.drain_pending().await;
    }

    // ----- Phase 5: read path integration via pending_renames -----

    #[tokio::test]
    async fn pending_visible_to_read_immediately() {
        let tmp = TempDir::new().unwrap();
        let mut disk = LocalVolume::new(tmp.path()).unwrap();
        disk.enable_write_pool(4).await.unwrap();

        let meta = test_meta(0);
        let payload = b"phase5 read after write";
        disk.write_shard("bucket", "imm", payload, &meta).await.unwrap();
        // No drain -- worker may not have run yet.
        let (got, got_meta) = disk.read_shard("bucket", "imm").await.unwrap();
        assert_eq!(got, payload);
        assert_eq!(got_meta.size, meta.size);
    }

    #[tokio::test]
    async fn pending_visible_to_stat_immediately_zero_disk() {
        let tmp = TempDir::new().unwrap();
        let mut disk = LocalVolume::new(tmp.path()).unwrap();
        disk.enable_write_pool(4).await.unwrap();

        let meta = test_meta(0);
        disk.write_shard("bucket", "stat", b"x", &meta).await.unwrap();
        // No drain -- worker may not have run yet.
        let got_meta = disk.stat_object("bucket", "stat").await.unwrap();
        assert_eq!(got_meta.size, meta.size);
        assert_eq!(got_meta.etag, meta.etag);
    }

    #[tokio::test]
    async fn pending_visible_to_list() {
        let tmp = TempDir::new().unwrap();
        let mut disk = LocalVolume::new(tmp.path()).unwrap();
        disk.enable_write_pool(4).await.unwrap();

        let meta = test_meta(0);
        disk.write_shard("bucket", "a.txt", b"a", &meta).await.unwrap();
        disk.write_shard("bucket", "b.txt", b"b", &meta).await.unwrap();
        // No drain.
        let keys = disk.list_objects("bucket", "").await.unwrap();
        assert!(keys.contains(&"a.txt".to_string()));
        assert!(keys.contains(&"b.txt".to_string()));
    }

    #[tokio::test]
    async fn delete_pending_cancels_rename() {
        let tmp = TempDir::new().unwrap();
        let mut disk = LocalVolume::new(tmp.path()).unwrap();
        disk.enable_write_pool(4).await.unwrap();

        let meta = test_meta(0);
        disk.write_shard("bucket", "doomed", b"data", &meta).await.unwrap();
        // Immediately delete -- the worker hasn't necessarily run.
        disk.delete_object("bucket", "doomed").await.unwrap();
        // Drain to let any in-flight worker activity settle.
        disk.drain_pending().await;

        // Destination should NOT exist
        let dest = pathing::object_dir(&tmp.path(), "bucket", "doomed").unwrap();
        let shard_path = dest.join("shard.dat");
        let meta_path = dest.join("meta.json");
        assert!(!shard_path.exists(), "shard.dat should not exist after delete-cancel");
        assert!(!meta_path.exists(), "meta.json should not exist after delete-cancel");

        // Read should 404
        let result = disk.read_shard("bucket", "doomed").await;
        assert!(result.is_err());
    }
}
