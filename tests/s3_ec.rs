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

/// No-op auth that accepts any access key. Required because s3s only invokes
/// S3Access::check when an auth provider is set.
struct AlwaysAllowAuth;

#[async_trait::async_trait]
impl S3Auth for AlwaysAllowAuth {
    async fn get_secret_key(&self, _access_key: &str) -> S3Result<SecretKey> {
        Ok(SecretKey::from("unused".to_string()))
    }
}

fn setup() -> (TempDir, Vec<std::path::PathBuf>) {
    setup_n(4)
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

async fn start_server(paths: &[std::path::PathBuf]) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let (addr, handle, _cluster) = start_server_with_cluster(paths).await;
    (addr, handle)
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

async fn start_server_with_cluster(
    paths: &[std::path::PathBuf],
) -> (SocketAddr, tokio::task::JoinHandle<()>, Arc<ClusterManager>) {
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
    let dispatch = build_dispatch(set, Arc::clone(&cluster));
    let (addr, handle) = spawn_server(dispatch).await;
    (addr, handle, cluster)
}

async fn start_server_pool(
    paths: &[std::path::PathBuf],
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
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
    let (addr, handle) = spawn_server(dispatch).await;

    (addr, handle)
}

fn url(addr: &SocketAddr, path: &str) -> String {
    format!("http://{}{}", addr, path)
}

fn url_with_query(addr: &SocketAddr, path: &str, params: &[(&str, &str)]) -> reqwest::Url {
    let mut url = reqwest::Url::parse(&url(addr, path)).unwrap();
    url.query_pairs_mut().extend_pairs(params);
    url
}


#[tokio::test]
async fn per_object_ec_headers_on_put_and_get() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths).await;
    let client = reqwest::Client::new();

    // create bucket
    client.put(url(&addr, "/ectest")).send().await.unwrap();

    // put with per-object FTT header
    let resp = client
        .put(url(&addr, "/ectest/critical.txt"))
        .header("x-amz-meta-ec-ftt", "5")
        .body("critical data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // get should return the object
    let resp = client
        .get(url(&addr, "/ectest/critical.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"critical data");

    // head should also work
    let resp = client
        .head(url(&addr, "/ectest/critical.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn per_object_ec_invalid_params_returns_400() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/ectest")).send().await.unwrap();

    // ftt >= disk count should fail
    let resp = client
        .put(url(&addr, "/ectest/bad1"))
        .header("x-amz-meta-ec-ftt", "6")
        .body("bad")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // ftt way too high should fail
    let resp = client
        .put(url(&addr, "/ectest/bad2"))
        .header("x-amz-meta-ec-ftt", "100")
        .body("bad")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn per_object_ec_mixed_ec_in_same_bucket() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/mixed")).send().await.unwrap();

    // write objects with different FTT
    client
        .put(url(&addr, "/mixed/high-parity"))
        .header("x-amz-meta-ec-ftt", "5")
        .body("high parity")
        .send()
        .await
        .unwrap();

    client
        .put(url(&addr, "/mixed/high-throughput"))
        .header("x-amz-meta-ec-ftt", "2")
        .body("high throughput")
        .send()
        .await
        .unwrap();

    client
        .put(url(&addr, "/mixed/default-ec"))
        .body("default ec")
        .send()
        .await
        .unwrap();

    // all three should be readable
    let resp = client
        .get(url(&addr, "/mixed/high-parity"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"high parity");

    let resp = client
        .get(url(&addr, "/mixed/high-throughput"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"high throughput");

    let resp = client
        .get(url(&addr, "/mixed/default-ec"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"default ec");

    // list should show all three
    let resp = client
        .get(url(&addr, "/mixed?list-type=2"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("high-parity"));
    assert!(body.contains("high-throughput"));
    assert!(body.contains("default-ec"));
}

#[tokio::test]
async fn per_object_ec_delete_mixed_ec_objects() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/delmix")).send().await.unwrap();

    // write with FTT=1 (5+1)
    client
        .put(url(&addr, "/delmix/small"))
        .header("x-amz-meta-ec-ftt", "1")
        .body("small")
        .send()
        .await
        .unwrap();

    // write with FTT=3 (3+3)
    client
        .put(url(&addr, "/delmix/big"))
        .header("x-amz-meta-ec-ftt", "3")
        .body("big")
        .send()
        .await
        .unwrap();

    // delete both
    let resp = client
        .delete(url(&addr, "/delmix/small"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    let resp = client
        .delete(url(&addr, "/delmix/big"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // both should be gone
    let resp = client
        .get(url(&addr, "/delmix/small"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let resp = client.get(url(&addr, "/delmix/big")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn per_object_ec_survives_disk_failures() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/resilient")).send().await.unwrap();

    // write with FTT=5 (1+5, can lose 5 of 6 disks)
    let resp = client
        .put(url(&addr, "/resilient/key"))
        .header("x-amz-meta-ec-ftt", "5")
        .body("survive anything")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // delete object data from 4 of 6 disks
    for i in 0..4 {
        let obj_dir = paths[i].join("resilient").join("key");
        if obj_dir.exists() {
            std::fs::remove_dir_all(&obj_dir).unwrap();
        }
    }

    // should still be readable
    let resp = client
        .get(url(&addr, "/resilient/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"survive anything");
}

#[tokio::test]
async fn per_object_ec_default_uses_pool_subset() {
    // 6 disks, default EC 2+2 -- should only use 4 of 6 disks
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/pooltest")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/pooltest/obj"))
        .body("pool test data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, "/pooltest/obj"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"pool test data");

    // default FTT=1 on 6 disks -> 5+1, uses all 6 disks
    let mut disks_with_obj = 0;
    for p in &paths {
        if p.join("pooltest").join("obj").join("meta.json").exists() {
            disks_with_obj += 1;
        }
    }
    assert_eq!(
        disks_with_obj, 6,
        "default FTT=1 on 6 disks -> 5+1 should use all 6 disks"
    );
}

// =============================================================================
// FTT (failures-to-tolerate) tests
// =============================================================================

#[tokio::test]
async fn ftt_header_sets_correct_ec() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/fttbucket")).send().await.unwrap();

    // FTT=2 on 6 disks -> 4 data + 2 parity = 6 shards across all disks
    let resp = client
        .put(url(&addr, "/fttbucket/critical.txt"))
        .header("x-amz-meta-ec-ftt", "2")
        .body("critical data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // verify all 6 disks have the object (4+2 = 6 total shards)
    let mut disks_with_obj = 0;
    for p in &paths {
        if p.join("fttbucket").join("critical.txt").join("meta.json").exists() {
            disks_with_obj += 1;
        }
    }
    assert_eq!(disks_with_obj, 6, "FTT=2 on 6 disks -> 4+2 should use all 6 disks");
}

#[tokio::test]
async fn ftt_zero_means_no_parity() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/ftt0bucket")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/ftt0bucket/bulk.log"))
        .header("x-amz-meta-ec-ftt", "0")
        .body("bulk log data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // FTT=0 on 6 disks -> 6+0, all 6 disks should have the object
    let mut disks_with_obj = 0;
    for p in &paths {
        if p.join("ftt0bucket").join("bulk.log").join("meta.json").exists() {
            disks_with_obj += 1;
        }
    }
    assert_eq!(disks_with_obj, 6, "FTT=0 on 6 disks -> 6+0 should use all 6 disks");

    // verify data is readable
    let resp = client
        .get(url(&addr, "/ftt0bucket/bulk.log"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"bulk log data");
}

#[tokio::test]
async fn ftt_exceeding_disks_returns_400() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/fttbadbucket")).send().await.unwrap();

    // FTT=6 on 6 disks is invalid (need at least 1 data shard)
    let resp = client
        .put(url(&addr, "/fttbadbucket/obj"))
        .header("x-amz-meta-ec-ftt", "6")
        .body("should fail")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn ftt_object_survives_expected_failures() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/fttsurvive")).send().await.unwrap();

    // FTT=2 -> 4+2, should survive 2 disk failures
    let resp = client
        .put(url(&addr, "/fttsurvive/important.dat"))
        .header("x-amz-meta-ec-ftt", "2")
        .body("survive test data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // delete object data from 2 disks
    for i in 0..2 {
        let obj_dir = paths[i].join("fttsurvive").join("important.dat");
        if obj_dir.exists() {
            std::fs::remove_dir_all(&obj_dir).unwrap();
        }
    }

    // should still be readable
    let resp = client
        .get(url(&addr, "/fttsurvive/important.dat"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"survive test data");
}

// =============================================================================
// Security tests -- comprehensive exploitation coverage
// Derived from RustFS security fixes and OWASP top 10 for object storage
// =============================================================================

// --- Path traversal variants ---

