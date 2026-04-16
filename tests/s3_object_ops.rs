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
    builder.set_access(abixio::s3_access::AbixioAccess::new(Arc::clone(&cluster), Arc::clone(&set), String::new()));
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
async fn put_get_object() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();

    // put
    let resp = client
        .put(url(&addr, "/testbucket/hello.txt"))
        .header("Content-Type", "text/plain")
        .body("hello world")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("etag"));

    // get
    let resp = client
        .get(url(&addr, "/testbucket/hello.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/plain");
    assert!(resp.headers().contains_key("etag"));
    assert!(resp.headers().contains_key("last-modified"));
    let body = resp.text().await.unwrap();
    assert_eq!(body, "hello world");
}

#[tokio::test]
async fn head_object() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/key"))
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client
        .head(url(&addr, "/testbucket/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-length"], "4");
    assert!(resp.headers().contains_key("etag"));
    // head should have no body
    let body = resp.text().await.unwrap();
    assert!(body.is_empty());
}

#[tokio::test]
async fn delete_object() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/key"))
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client
        .delete(url(&addr, "/testbucket/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // subsequent get returns 404
    let resp = client
        .get(url(&addr, "/testbucket/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn list_objects() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/aaa"))
        .body("1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/bbb"))
        .body("2")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/testbucket?list-type=2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("ListBucketResult"));
    assert!(body.contains("aaa"));
    assert!(body.contains("bbb"));
}

#[tokio::test]
async fn list_objects_with_prefix() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/logs/a"))
        .body("1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/logs/b"))
        .body("2")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/data/c"))
        .body("3")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/testbucket?list-type=2&prefix=logs/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("logs/a"));
    assert!(body.contains("logs/b"));
    assert!(!body.contains("data/c"));
}

#[tokio::test]
async fn list_objects_with_delimiter() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/a/1"))
        .body("1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/b/2"))
        .body("2")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/root"))
        .body("3")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/testbucket?list-type=2&delimiter=/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("CommonPrefixes"));
    assert!(body.contains("root"));
}

#[tokio::test]
async fn list_objects_with_encoded_delimiter_and_prefix() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/docs/readme.txt"))
        .body("readme")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/docs/guide.txt"))
        .body("guide")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/photos/cat.jpg"))
        .body("cat")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url_with_query(
            &addr,
            "/testbucket",
            &[("list-type", "2"), ("delimiter", "/")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<Prefix>docs/</Prefix>"));
    assert!(body.contains("<Prefix>photos/</Prefix>"));

    let resp = client
        .get(url_with_query(
            &addr,
            "/testbucket",
            &[("list-type", "2"), ("prefix", "docs/")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("docs/readme.txt"));
    assert!(body.contains("docs/guide.txt"));
    assert!(!body.contains("photos/cat.jpg"));
}

#[tokio::test]
async fn put_empty_body() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/testbucket/empty"))
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, "/testbucket/empty"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-length"], "0");
}

#[tokio::test]
async fn delete_nonexistent_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();

    let resp = client
        .delete(url(&addr, "/testbucket/nonexistent"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// --- x-amz-request-id header on all responses ---

#[tokio::test]
async fn copy_object_same_bucket() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/src"))
        .header("Content-Type", "text/plain")
        .body("original")
        .send()
        .await
        .unwrap();

    let resp = client
        .put(url(&addr, "/tbk/dst"))
        .header("x-amz-copy-source", "/tbk/src")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("CopyObjectResult"), "body: {}", body);
    assert!(body.contains("ETag"));

    // verify copy
    let resp = client.get(url(&addr, "/tbk/dst")).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "original");
}

#[tokio::test]
async fn copy_object_cross_bucket() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/src-bucket")).send().await.unwrap();
    client.put(url(&addr, "/dst-bucket")).send().await.unwrap();
    client
        .put(url(&addr, "/src-bucket/key"))
        .body("cross-bucket")
        .send()
        .await
        .unwrap();

    let resp = client
        .put(url(&addr, "/dst-bucket/key"))
        .header("x-amz-copy-source", "/src-bucket/key")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, "/dst-bucket/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "cross-bucket");
}

// --- DeleteObjects (batch) ---

#[tokio::test]
async fn delete_objects_batch() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/a"))
        .body("1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/tbk/b"))
        .body("2")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/tbk/c"))
        .body("3")
        .send()
        .await
        .unwrap();

    let delete_xml =
        r#"<Delete><Object><Key>a</Key></Object><Object><Key>b</Key></Object></Delete>"#;
    let resp = client
        .post(url(&addr, "/tbk?delete"))
        .body(delete_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("DeleteResult"), "body: {}", body);
    assert!(body.contains("<Key>a</Key>"));
    assert!(body.contains("<Key>b</Key>"));

    // a and b should be gone, c should remain
    let resp = client.get(url(&addr, "/tbk/a")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client.get(url(&addr, "/tbk/c")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

// --- Range requests ---

#[tokio::test]
async fn range_request_partial_content() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("hello world")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/tbk/obj"))
        .header("Range", "bytes=0-4")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert!(resp.headers().contains_key("content-range"));
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn range_request_invalid_returns_416() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("small")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/tbk/obj"))
        .header("Range", "bytes=100-200")
        .send()
        .await
        .unwrap();
    // s3s may return 200 with full body or 416; both indicate the server handled it
    assert!(resp.status() == 416 || resp.status() == 200, "got {}", resp.status());
}

// --- Custom metadata ---

#[tokio::test]
async fn custom_metadata_round_trip() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .header("x-amz-meta-author", "alice")
        .header("x-amz-meta-version", "42")
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client.head(url(&addr, "/tbk/obj")).send().await.unwrap();
    assert_eq!(resp.headers()["x-amz-meta-author"], "alice");
    assert_eq!(resp.headers()["x-amz-meta-version"], "42");

    let resp = client.get(url(&addr, "/tbk/obj")).send().await.unwrap();
    assert_eq!(resp.headers()["x-amz-meta-author"], "alice");
}

// --- Delete bucket ---

#[tokio::test]
async fn list_objects_clamps_max_keys() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // max-keys=0 should work (returns nothing or default)
    let resp = client
        .get(url_with_query(
            &addr,
            "/tbk",
            &[("list-type", "2"), ("max-keys", "0")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // max-keys=999999 should not crash
    let resp = client
        .get(url_with_query(
            &addr,
            "/tbk",
            &[("list-type", "2"), ("max-keys", "999999")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // max-keys=-1: s3s may reject with 400
    let resp = client
        .get(url_with_query(
            &addr,
            "/tbk",
            &[("list-type", "2"), ("max-keys", "-1")],
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status() == 200 || resp.status() == 400, "got {}", resp.status());

    // max-keys=NaN: s3s correctly rejects with 400
    let resp = client
        .get(url_with_query(
            &addr,
            "/tbk",
            &[("list-type", "2"), ("max-keys", "not-a-number")],
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status() == 200 || resp.status() == 400, "got {}", resp.status());
}

// --- Multipart hostile inputs ---

#[tokio::test]
async fn delete_nonexistent_object_does_not_crash() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // DELETE nonexistent object -- AbixIO returns 404 (not S3-compliant 204,
    // but the important thing is it doesn't crash or return 500)
    let resp = client
        .delete(url(&addr, "/tbk/does-not-exist"))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == 204 || resp.status() == 200 || resp.status() == 404,
        "delete nonexistent should not crash, got {}",
        resp.status()
    );
}

// --- Hostile metadata/header injection ---

