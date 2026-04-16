//! Lifecycle enforcement engine. Runs on a timer, reads each bucket's
//! lifecycle configuration, evaluates rules per object version, and
//! carries out the resulting actions (delete, delete version, reap
//! expired delete markers, abort incomplete multipart uploads).

use std::sync::Arc;
use std::time::Duration;

use s3s::dto::{BucketLifecycleConfiguration, LifecycleRule};

use crate::storage::metadata::ListOptions;
use crate::storage::pathing;
use crate::storage::volume_pool::VolumePool;
use crate::storage::Store;

pub mod evaluator;
pub mod filter;
pub mod types;

pub use evaluator::Evaluator;
pub use types::{unix_now_secs, Action, Event, ObjectOpts};

/// Per-pass counters surfaced for logging and tests.
#[derive(Clone, Debug, Default)]
pub struct PassStats {
    pub buckets_scanned: usize,
    pub objects_examined: usize,
    pub deleted: usize,
    pub deleted_versions: usize,
    pub delete_markers_reaped: usize,
    pub multipart_aborted: usize,
}

/// Parse the on-disk lifecycle payload. The GetBucketLifecycle handler
/// stores a `Vec<LifecycleRule>` JSON blob (matching s3s DTO field
/// shapes); wrap it back up as a `BucketLifecycleConfiguration` so the
/// evaluator has the same view it would after a round trip through
/// the S3 API.
pub fn parse_lifecycle_config(json: &str) -> Option<BucketLifecycleConfiguration> {
    // Primary shape: bare Vec<LifecycleRule> from the PUT handler.
    if let Ok(rules) = serde_json::from_str::<Vec<LifecycleRule>>(json) {
        return Some(BucketLifecycleConfiguration { rules });
    }
    // Fallback: an already-wrapped BucketLifecycleConfiguration written
    // by a test or external tool.
    if let Ok(cfg) = serde_json::from_str::<BucketLifecycleConfiguration>(json) {
        return Some(cfg);
    }
    None
}

pub struct LifecycleEngine {
    store: Arc<VolumePool>,
}

impl LifecycleEngine {
    pub fn new(store: Arc<VolumePool>) -> Self {
        Self { store }
    }

    /// Execute a single enforcement pass across every bucket.
    pub async fn run_once(&self) -> PassStats {
        let mut stats = PassStats::default();
        let buckets = match self.store.list_buckets().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "lifecycle: list_buckets failed");
                return stats;
            }
        };

        for bucket in buckets {
            stats.buckets_scanned += 1;
            let settings = match self.store.get_bucket_settings(&bucket.name).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Some(ref lifecycle_json) = settings.lifecycle {
                if let Some(cfg) = parse_lifecycle_config(lifecycle_json) {
                    self.apply_bucket(&bucket.name, &cfg, &mut stats).await;
                    self.abort_stale_multiparts(&bucket.name, &cfg, &mut stats).await;
                }
            }
        }

        if stats.deleted + stats.deleted_versions + stats.delete_markers_reaped + stats.multipart_aborted > 0 {
            tracing::info!(
                buckets = stats.buckets_scanned,
                objects = stats.objects_examined,
                deleted = stats.deleted,
                deleted_versions = stats.deleted_versions,
                delete_markers_reaped = stats.delete_markers_reaped,
                multipart_aborted = stats.multipart_aborted,
                "lifecycle pass complete"
            );
        }
        stats
    }

    async fn apply_bucket(
        &self,
        bucket: &str,
        cfg: &BucketLifecycleConfiguration,
        stats: &mut PassStats,
    ) {
        // If the bucket has any lifecycle rules but all are Disabled,
        // skip the listing entirely. Evaluator would no-op but listing
        // is expensive.
        if cfg.rules.iter().all(|r| r.status.as_str() != s3s::dto::ExpirationStatus::ENABLED) {
            return;
        }

        let evaluator = Evaluator::new(cfg);
        let now = unix_now_secs();

        // Check for a versioned bucket. In the unversioned case
        // list_object_versions still works but the single "current"
        // entry has no version_id.
        let is_versioned = self.store
            .get_versioning_config(bucket)
            .await
            .ok()
            .flatten()
            .map(|v| v.status == "Enabled" || v.status == "Suspended")
            .unwrap_or(false);

        // Unversioned path: walk list_objects.
        if !is_versioned {
            let opts = ListOptions { prefix: String::new(), ..Default::default() };
            let listing = match self.store.list_objects(bucket, opts).await {
                Ok(l) => l,
                Err(_) => return,
            };
            for info in listing.objects {
                stats.objects_examined += 1;
                let obj = ObjectOpts {
                    name: info.key.clone(),
                    mod_time_unix_secs: info.created_at,
                    version_id: String::new(),
                    is_latest: true,
                    is_delete_marker: info.is_delete_marker,
                    num_versions: 1,
                    successor_mod_time_unix_secs: 0,
                    size: info.size,
                };
                let event = evaluator.eval(&obj, now, 0);
                self.apply_event(bucket, &obj, &event, stats).await;
            }
            return;
        }

        // Versioned path: walk list_object_versions, group by key.
        let groups = match self.store.list_object_versions(bucket, "").await {
            Ok(g) => g,
            Err(_) => return,
        };
        for (key, versions) in groups {
            let num_versions = versions.len();
            let mut newer_noncurrent = 0usize;
            for (i, meta) in versions.iter().enumerate() {
                stats.objects_examined += 1;
                let successor_mod_time = if i == 0 {
                    0
                } else {
                    versions[i - 1].created_at
                };
                let obj = ObjectOpts {
                    name: key.clone(),
                    mod_time_unix_secs: meta.created_at,
                    version_id: meta.version_id.clone(),
                    is_latest: meta.is_latest,
                    is_delete_marker: meta.is_delete_marker,
                    num_versions,
                    successor_mod_time_unix_secs: successor_mod_time,
                    size: meta.size,
                };
                let event = evaluator.eval(&obj, now, newer_noncurrent);
                let acted = self.apply_event(bucket, &obj, &event, stats).await;
                if !obj.is_latest && !acted {
                    newer_noncurrent += 1;
                }
            }
        }
    }

    async fn apply_event(
        &self,
        bucket: &str,
        obj: &ObjectOpts,
        event: &Event,
        stats: &mut PassStats,
    ) -> bool {
        match event.action {
            Action::None => false,
            Action::Delete => {
                // For an unversioned key this hard-deletes; for a
                // versioned latest we add a delete marker (S3 semantics).
                if obj.version_id.is_empty() {
                    if self.store.delete_object(bucket, &obj.name).await.is_ok() {
                        stats.deleted += 1;
                        return true;
                    }
                } else if self.store.add_delete_marker(bucket, &obj.name).await.is_ok() {
                    stats.deleted += 1;
                    return true;
                }
                false
            }
            Action::DeleteVersion => {
                if obj.version_id.is_empty() {
                    return false;
                }
                if self.store
                    .delete_object_version(bucket, &obj.name, &obj.version_id)
                    .await
                    .is_ok()
                {
                    stats.deleted_versions += 1;
                    return true;
                }
                false
            }
            Action::DeleteAllVersions => {
                if self.store.delete_object(bucket, &obj.name).await.is_ok() {
                    stats.deleted += 1;
                    return true;
                }
                false
            }
            Action::DeleteExpiredMarker => {
                if obj.version_id.is_empty() {
                    if self.store.delete_object(bucket, &obj.name).await.is_ok() {
                        stats.delete_markers_reaped += 1;
                        return true;
                    }
                } else if self.store
                    .delete_object_version(bucket, &obj.name, &obj.version_id)
                    .await
                    .is_ok()
                {
                    stats.delete_markers_reaped += 1;
                    return true;
                }
                false
            }
        }
    }

    /// Walk the multipart temp directory for every disk in the pool,
    /// aborting uploads older than any rule's
    /// `AbortIncompleteMultipartUpload.days_after_initiation`.
    async fn abort_stale_multiparts(
        &self,
        bucket: &str,
        cfg: &BucketLifecycleConfiguration,
        stats: &mut PassStats,
    ) {
        // The shortest days setting among matching enabled rules is the
        // one that applies. We pick the min across all enabled rules
        // (not per-object since multipart uploads aren't filtered by
        // prefix in abixio's layout).
        let min_days: Option<i32> = cfg.rules.iter()
            .filter(|r| r.status.as_str() == s3s::dto::ExpirationStatus::ENABLED)
            .filter_map(|r| r.abort_incomplete_multipart_upload.as_ref())
            .filter_map(|a| a.days_after_initiation)
            .min();
        let Some(days) = min_days else { return };
        if days < 0 {
            return;
        }
        let cutoff_secs = unix_now_secs().saturating_sub((days as u64).saturating_mul(86_400));

        // Each disk stores its own multipart temp dir; walk all of them
        // and aggregate upload ids. Aborting is idempotent, so racing
        // multiple disks for the same upload id is fine.
        use std::collections::BTreeSet;
        let mut to_abort: BTreeSet<(String, String)> = BTreeSet::new();
        for disk in self.store.disks() {
            let info = disk.info();
            if info.backend_type != "local" {
                continue;
            }
            let root = std::path::PathBuf::from(
                info.label.strip_prefix("local:").unwrap_or(&info.label),
            );
            let bdir = match pathing::multipart_bucket_dir(&root, bucket) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !bdir.is_dir() {
                continue;
            }
            let mut key_iter = match tokio::fs::read_dir(&bdir).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Ok(Some(key_entry)) = key_iter.next_entry().await {
                if !key_entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let key = key_entry.file_name().to_string_lossy().to_string();
                let mut upload_iter = match tokio::fs::read_dir(key_entry.path()).await {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                while let Ok(Some(u_entry)) = upload_iter.next_entry().await {
                    if !u_entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let meta = match u_entry.metadata().await {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    let modified_secs = meta.modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    if modified_secs > 0 && modified_secs <= cutoff_secs {
                        let upload_id = u_entry.file_name().to_string_lossy().to_string();
                        to_abort.insert((key.clone(), upload_id));
                    }
                }
            }
        }

        for (key, upload_id) in to_abort {
            // Best-effort remove the temp dir on each disk. We don't
            // have an abort-multipart method on Store, but cleaning up
            // the disk dirs achieves the same result and matches what
            // AbortMultipartUpload handler does.
            for disk in self.store.disks() {
                let info = disk.info();
                if info.backend_type != "local" {
                    continue;
                }
                let root = std::path::PathBuf::from(
                info.label.strip_prefix("local:").unwrap_or(&info.label),
            );
                if let Ok(upload_dir) = pathing::multipart_upload_dir(&root, bucket, &key, &upload_id) {
                    let _ = tokio::fs::remove_dir_all(&upload_dir).await;
                }
            }
            stats.multipart_aborted += 1;
        }
    }
}

/// Background loop: run the engine on a fixed interval, stopping when
/// `shutdown_rx` fires.
pub async fn lifecycle_loop(
    engine: LifecycleEngine,
    interval: Duration,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // first tick fires immediately; skip so startup isn't slowed
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let _ = engine.run_once().await;
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
}
