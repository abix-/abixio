//! End-to-end lifecycle enforcement tests. Drive the engine directly
//! against a `VolumePool` so the test is deterministic and does not
//! depend on the background timer.

use std::sync::Arc;

use abixio::lifecycle::LifecycleEngine;
use abixio::storage::Backend;
use abixio::storage::local_volume::LocalVolume;
use abixio::storage::metadata::{BucketSettings, PutOptions, VersioningConfig};
use abixio::storage::pathing;
use abixio::storage::volume_pool::VolumePool;
use abixio::storage::Store;
use tempfile::TempDir;

fn setup(n: usize) -> (TempDir, Vec<std::path::PathBuf>) {
    let base = TempDir::new().unwrap();
    let mut paths = Vec::new();
    for i in 0..n {
        let p = base.path().join(format!("d{}", i));
        std::fs::create_dir_all(&p).unwrap();
        paths.push(p);
    }
    (base, paths)
}

fn make_set(paths: &[std::path::PathBuf]) -> Arc<VolumePool> {
    let backends: Vec<Box<dyn Backend>> = paths
        .iter()
        .map(|p| Box::new(LocalVolume::new(p.as_path()).unwrap()) as Box<dyn Backend>)
        .collect();
    Arc::new(VolumePool::new(backends).unwrap())
}

/// Rewrite an object's `created_at` to simulate it being N days old.
/// Directly edits every `meta.json` shard on disk so the engine sees
/// an expired object on its next pass.
fn back_date_object(paths: &[std::path::PathBuf], bucket: &str, key: &str, age_secs: u64) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let target = now - age_secs;
    for p in paths {
        let path = match pathing::object_meta_path(p, bucket, key) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let mut mf: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if let Some(versions) = mf.get_mut("versions").and_then(|v| v.as_array_mut()) {
            for v in versions {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("created_at".to_string(), serde_json::json!(target));
                }
            }
        }
        std::fs::write(&path, serde_json::to_vec(&mf).unwrap()).unwrap();
    }
}

fn lifecycle_json_with_prefix_days(prefix: &str, days: i32) -> String {
    // Shape matches what the `PutBucketLifecycleConfiguration` handler
    // stores: a bare `Vec<LifecycleRule>` with s3s DTO field names.
    serde_json::json!([{
        "id": "expire",
        "status": "Enabled",
        "filter": { "prefix": prefix },
        "expiration": { "days": days }
    }])
    .to_string()
}

#[tokio::test]
async fn expiration_deletes_old_object_past_prefix() {
    let (_base, paths) = setup(4);
    let set = make_set(&paths);
    set.make_bucket("lc").await.unwrap();

    let opts = PutOptions::default();
    set.put_object("lc", "tmp/a", b"aaa", opts.clone()).await.unwrap();
    set.put_object("lc", "keep/b", b"bbb", opts).await.unwrap();

    // age tmp/a to 10 days old; keep/b stays fresh
    back_date_object(&paths, "lc", "tmp/a", 10 * 86_400);

    // attach lifecycle: expire tmp/* after 5 days
    let mut settings = set.get_bucket_settings("lc").await.unwrap_or_default();
    settings.lifecycle = Some(lifecycle_json_with_prefix_days("tmp/", 5));
    set.set_bucket_settings("lc", &settings).await.unwrap();

    let engine = LifecycleEngine::new(Arc::clone(&set));
    let stats = engine.run_once().await;

    assert!(stats.deleted >= 1, "expected deletion, stats: {:?}", stats);
    assert!(
        set.head_object("lc", "tmp/a").await.is_err(),
        "tmp/a should be gone"
    );
    assert!(
        set.head_object("lc", "keep/b").await.is_ok(),
        "keep/b should remain"
    );
}

#[tokio::test]
async fn disabled_rule_is_ignored() {
    let (_base, paths) = setup(4);
    let set = make_set(&paths);
    set.make_bucket("lc").await.unwrap();

    let opts = PutOptions::default();
    set.put_object("lc", "old", b"x", opts).await.unwrap();
    back_date_object(&paths, "lc", "old", 30 * 86_400);

    let cfg = serde_json::json!([{
        "id": "disabled-rule",
        "status": "Disabled",
        "filter": { "prefix": "" },
        "expiration": { "days": 1 }
    }]);
    let settings = BucketSettings {
        lifecycle: Some(cfg.to_string()),
        ..Default::default()
    };
    set.set_bucket_settings("lc", &settings).await.unwrap();

    let engine = LifecycleEngine::new(Arc::clone(&set));
    let stats = engine.run_once().await;
    assert_eq!(stats.deleted, 0, "disabled rules must not delete");
    assert!(set.head_object("lc", "old").await.is_ok());
}

#[tokio::test]
async fn versioned_expiration_adds_delete_marker() {
    let (_base, paths) = setup(4);
    let set = make_set(&paths);
    set.make_bucket("lcv").await.unwrap();
    set.set_versioning_config("lcv", &VersioningConfig { status: "Enabled".to_string() })
        .await.unwrap();

    let opts = PutOptions::default();
    let vid = "11111111-1111-1111-1111-111111111111";
    set.put_object_versioned("lcv", "k", b"v1", opts, vid).await.unwrap();
    back_date_object(&paths, "lcv", "k", 10 * 86_400);

    let mut settings = set.get_bucket_settings("lcv").await.unwrap_or_default();
    settings.lifecycle = Some(lifecycle_json_with_prefix_days("", 1));
    set.set_bucket_settings("lcv", &settings).await.unwrap();

    let engine = LifecycleEngine::new(Arc::clone(&set));
    let stats = engine.run_once().await;

    // Versioned delete adds a delete marker, not an immediate hard delete.
    assert!(stats.deleted >= 1, "expected delete-marker op, stats: {:?}", stats);
    // head_object returns 404 (because the latest version is now a delete marker)
    // but the original version remains addressable by version_id.
    let versions = set.list_object_versions("lcv", "").await.unwrap();
    let (_, list) = versions.into_iter().find(|(k, _)| k == "k").unwrap();
    assert!(list.iter().any(|m| m.version_id == vid));
    assert!(list.iter().any(|m| m.is_delete_marker));
}

#[tokio::test]
async fn abort_incomplete_multipart_after_days() {
    let (_base, paths) = setup(4);
    let set = make_set(&paths);
    set.make_bucket("mpb").await.unwrap();

    // Create a stale multipart upload dir on every disk with a
    // modification time 10 days in the past.
    let upload_id = uuid::Uuid::new_v4().to_string();
    let stale_when = std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 86_400);
    for p in &paths {
        let upload_dir = pathing::multipart_upload_dir(p, "mpb", "bigkey", &upload_id).unwrap();
        std::fs::create_dir_all(&upload_dir).unwrap();
        // write a sentinel file so remove_dir_all has something to remove
        std::fs::write(upload_dir.join(".created"), b"x").unwrap();
        let _ = filetime::set_file_mtime(&upload_dir, filetime::FileTime::from_system_time(stale_when));
    }

    let cfg = serde_json::json!([{
        "id": "abort",
        "status": "Enabled",
        "filter": { "prefix": "" },
        "abort_incomplete_multipart_upload": { "days_after_initiation": 1 }
    }]);
    let mut settings = set.get_bucket_settings("mpb").await.unwrap_or_default();
    settings.lifecycle = Some(cfg.to_string());
    set.set_bucket_settings("mpb", &settings).await.unwrap();

    let engine = LifecycleEngine::new(Arc::clone(&set));
    let stats = engine.run_once().await;

    assert!(stats.multipart_aborted >= 1, "expected an abort, stats: {:?}", stats);
    for p in &paths {
        let upload_dir = pathing::multipart_upload_dir(p, "mpb", "bigkey", &upload_id).unwrap();
        assert!(!upload_dir.exists(), "upload dir should be gone: {:?}", upload_dir);
    }
}
