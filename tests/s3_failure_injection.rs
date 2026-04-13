use std::net::SocketAddr;
use std::sync::Arc;

use abixio::cluster::{ClusterConfig, ClusterManager};
use abixio::s3_route::AbixioDispatch;
use abixio::storage::Backend;
use abixio::storage::local_volume::LocalVolume;
use abixio::storage::volume_pool::VolumePool;
use s3s::S3Result;
use s3s::auth::{S3Auth, SecretKey};
use tempfile::TempDir;

struct AlwaysAllowAuth;

#[async_trait::async_trait]
impl S3Auth for AlwaysAllowAuth {
    async fn get_secret_key(&self, _access_key: &str) -> S3Result<SecretKey> {
        Ok(SecretKey::from("unused".to_string()))
    }
}

fn setup_n(n: usize) -> (TempDir, Vec<std::path::PathBuf>) {
    let base = TempDir::new().unwrap();
    let mut paths = Vec::new();
    for i in 0..n {
        let p = base.path().join(format!("d{}", i));
        std::fs::create_dir_all(&p).unwrap();
        paths.push(p);
    }
    (base, paths)
}

fn build_dispatch(set: Arc<VolumePool>, cluster: Arc<ClusterManager>) -> Arc<AbixioDispatch> {
    let s3 = abixio::s3_service::AbixioS3::new(Arc::clone(&set), Arc::clone(&cluster));
    let mut builder = s3s::service::S3ServiceBuilder::new(s3);
    builder.set_auth(AlwaysAllowAuth);
    builder.set_validation(abixio::s3_service::RelaxedNameValidation);
    builder.set_access(abixio::s3_access::AbixioAccess::new(Arc::clone(&cluster)));
    let s3_service = builder.build();
    Arc::new(AbixioDispatch::new(s3_service, None, None))
}

async fn spawn_server(dispatch: Arc<AbixioDispatch>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = hyper_util::rt::TokioIo::new(stream);
            let dispatch = dispatch.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let dispatch = dispatch.clone();
                    async move { Ok::<_, hyper::Error>(dispatch.dispatch(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    (addr, handle)
}

async fn start_server(paths: &[std::path::PathBuf]) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let backends: Vec<Box<dyn Backend>> = paths
        .iter()
        .map(|p| Box::new(LocalVolume::new(p.as_path()).unwrap()) as Box<dyn Backend>)
        .collect();
    let set = Arc::new(VolumePool::new(backends).unwrap());
    let cluster = Arc::new(
        ClusterManager::new(ClusterConfig {
            node_id: "test-node".to_string(),
            advertise_s3: "http://127.0.0.1:0".to_string(),
            advertise_cluster: "http://127.0.0.1:0".to_string(),
            nodes: Vec::new(),
            access_key: String::new(),
            secret_key: String::new(),
            no_auth: true,
            disk_paths: paths.to_vec(),
        })
        .unwrap(),
    );
    let dispatch = build_dispatch(set, cluster);
    spawn_server(dispatch).await
}

fn url(addr: &SocketAddr, path: &str) -> String {
    format!("http://{}{}", addr, path)
}

// Default EC on 4 disks: FTT=1 -> 3+1 (3 data, 1 parity)
// Can tolerate exactly 1 failed/corrupted shard.
//
// For 2-failure tolerance, use x-amz-meta-ec-ftt: 2 -> 2+2
//
// Objects <= 64KB are inlined in meta.json (no shard.dat file).
// Bitrot tests use 128KB payloads to ensure data lives in shard.dat.

fn large_payload() -> Vec<u8> {
    vec![0x42u8; 128 * 1024] // 128KB, above 64KB inline threshold
}

// -----------------------------------------------------------------------
// Bitrot: corrupt shard data, verify RS reconstruction through S3 GET
// -----------------------------------------------------------------------

#[tokio::test]
async fn bitrot_one_shard_corrupted_recovers() {
    // 4 disks, default 3+1: corrupt 1 shard, RS reconstructs
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    let resp = client
        .put(url(&addr, "/b/key"))
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // corrupt shard.dat on disk 0
    let shard = paths[0].join("b").join("key").join("shard.dat");
    if shard.exists() {
        std::fs::write(&shard, b"CORRUPTED GARBAGE DATA").unwrap();
    }

    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), payload.as_slice());
}

#[tokio::test]
async fn bitrot_two_shards_corrupted_exceeds_default_tolerance() {
    // 4 disks, default 3+1: 2 corrupted exceeds FTT=1
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .body(payload.clone())
        .send()
        .await
        .unwrap();

    // corrupt 2 of 4 shards
    for i in 0..2 {
        let shard = paths[i].join("b").join("key").join("shard.dat");
        if shard.exists() {
            std::fs::write(&shard, b"BAD").unwrap();
        }
    }

    // 503 = read quorum not met (3 data shards needed, only 2 healthy)
    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn bitrot_two_shards_with_ftt2_recovers() {
    // 4 disks, FTT=2 -> 2+2: can tolerate 2 corrupted shards
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .header("x-amz-meta-ec-ftt", "2")
        .body(payload.clone())
        .send()
        .await
        .unwrap();

    // corrupt 2 of 4 shards -- within FTT=2 tolerance
    for i in 0..2 {
        let shard = paths[i].join("b").join("key").join("shard.dat");
        if shard.exists() {
            std::fs::write(&shard, b"BAD").unwrap();
        }
    }

    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), payload.as_slice());
}

// -----------------------------------------------------------------------
// Shard deletion: remove entire shard directories
// -----------------------------------------------------------------------

#[tokio::test]
async fn deleted_one_shard_recovers() {
    // 4 disks, default 3+1: delete 1 shard dir, RS reconstructs
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .body(payload.clone())
        .send()
        .await
        .unwrap();

    // delete 1 shard directory
    let obj_dir = paths[0].join("b").join("key");
    if obj_dir.exists() {
        std::fs::remove_dir_all(&obj_dir).unwrap();
    }

    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), payload.as_slice());
}

#[tokio::test]
async fn deleted_two_shards_exceeds_default_tolerance() {
    // 4 disks, default 3+1: delete 2 exceeds FTT=1
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .body(payload)
        .send()
        .await
        .unwrap();

    let mut deleted = 0;
    for i in 0..4 {
        let obj_dir = paths[i].join("b").join("key");
        if obj_dir.exists() && deleted < 2 {
            std::fs::remove_dir_all(&obj_dir).unwrap();
            deleted += 1;
        }
    }

    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn deleted_two_shards_with_ftt2_recovers() {
    // 4 disks, FTT=2 -> 2+2: delete 2 shards, still readable
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .header("x-amz-meta-ec-ftt", "2")
        .body(payload.clone())
        .send()
        .await
        .unwrap();

    let mut deleted = 0;
    for i in 0..4 {
        let obj_dir = paths[i].join("b").join("key");
        if obj_dir.exists() && deleted < 2 {
            std::fs::remove_dir_all(&obj_dir).unwrap();
            deleted += 1;
        }
    }

    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), payload.as_slice());
}

// -----------------------------------------------------------------------
// Metadata corruption
// -----------------------------------------------------------------------

#[tokio::test]
async fn corrupted_metadata_on_one_disk_get_succeeds() {
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .body(payload.clone())
        .send()
        .await
        .unwrap();

    let meta = paths[0].join("b").join("key").join("meta.json");
    if meta.exists() {
        std::fs::write(&meta, b"NOT VALID JSON").unwrap();
    }

    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), payload.as_slice());
}

// -----------------------------------------------------------------------
// Volume loss: entire bucket dir removed from one disk
// -----------------------------------------------------------------------

#[tokio::test]
async fn entire_volume_bucket_removed_get_succeeds() {
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .body(payload.clone())
        .send()
        .await
        .unwrap();

    let bucket_dir = paths[0].join("b");
    if bucket_dir.exists() {
        std::fs::remove_dir_all(&bucket_dir).unwrap();
    }

    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), payload.as_slice());
}

// -----------------------------------------------------------------------
// Mixed corruption: shard + metadata on same disk
// -----------------------------------------------------------------------

#[tokio::test]
async fn both_shard_and_meta_corrupted_on_one_disk_get_succeeds() {
    // default 3+1: one fully corrupted disk is within tolerance
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .body(payload.clone())
        .send()
        .await
        .unwrap();

    let shard = paths[0].join("b").join("key").join("shard.dat");
    let meta = paths[0].join("b").join("key").join("meta.json");
    if shard.exists() {
        std::fs::write(&shard, b"BAD SHARD").unwrap();
    }
    if meta.exists() {
        std::fs::write(&meta, b"BAD META").unwrap();
    }

    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), payload.as_slice());
}

// -----------------------------------------------------------------------
// 1+0 (no parity): any corruption is fatal
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore] // 1+0 path intentionally skips checksum (zero-copy mmap, no parity to reconstruct from). separate design decision.
async fn no_parity_corruption_serves_corrupted_data() {
    // 1 disk, FTT=0 -> 1+0: no parity, no checksum on read.
    // corruption is served as-is (zero-copy mmap fast path).
    let (_base, paths) = setup_n(1);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .body(payload.clone())
        .send()
        .await
        .unwrap();

    // verify clean read first
    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), payload.as_slice());

    // corrupt the only shard
    let shard = paths[0].join("b").join("key").join("shard.dat");
    assert!(shard.exists(), "shard.dat should exist for large objects");
    std::fs::write(&shard, b"CORRUPTED").unwrap();

    // 1+0 fast path: mmap serves whatever is on disk, no checksum.
    // response succeeds but returns corrupted data.
    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert_ne!(body.as_ref(), payload.as_slice(), "data should be corrupted");
}

// -----------------------------------------------------------------------
// HEAD after corruption: metadata from healthy disks
// -----------------------------------------------------------------------

#[tokio::test]
async fn head_succeeds_after_one_shard_corruption() {
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let payload = large_payload();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .header("content-type", "application/octet-stream")
        .body(payload)
        .send()
        .await
        .unwrap();

    let shard = paths[0].join("b").join("key").join("shard.dat");
    if shard.exists() {
        std::fs::write(&shard, b"BAD").unwrap();
    }

    // HEAD reads metadata, not shard data
    let resp = client.head(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

// -----------------------------------------------------------------------
// Large object bitrot
// -----------------------------------------------------------------------

#[tokio::test]
async fn bitrot_large_object_reconstruction() {
    let (_base, paths) = setup_n(4);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/b")).send().await.unwrap();

    // 256KB payload (above 64KB small-object threshold)
    let payload = vec![0x42u8; 256 * 1024];
    let resp = client
        .put(url(&addr, "/b/big"))
        .header("content-length", payload.len().to_string())
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // corrupt shard on disk 0
    let shard = paths[0].join("b").join("big").join("shard.dat");
    if shard.exists() {
        std::fs::write(&shard, b"CORRUPTED LARGE SHARD").unwrap();
    }

    let resp = client.get(url(&addr, "/b/big")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), payload.as_slice());
}

// -----------------------------------------------------------------------
// High FTT on 6 disks: survive 4 of 6 failures (from s3_ec pattern)
// -----------------------------------------------------------------------

#[tokio::test]
async fn ftt5_survives_four_shard_deletions_on_six_disks() {
    // 6 disks, FTT=5 -> 1+5: can lose 5 of 6
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/b")).send().await.unwrap();
    client
        .put(url(&addr, "/b/key"))
        .header("x-amz-meta-ec-ftt", "5")
        .body("high ftt test")
        .send()
        .await
        .unwrap();

    // delete 4 of 6 shard dirs
    for i in 0..4 {
        let obj_dir = paths[i].join("b").join("key");
        if obj_dir.exists() {
            std::fs::remove_dir_all(&obj_dir).unwrap();
        }
    }

    let resp = client.get(url(&addr, "/b/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"high ftt test");
}
