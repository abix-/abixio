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
async fn versioning_disabled_by_default() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .get(url(&addr, "/tbk?versioning"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("VersioningConfiguration"),
        "expected XML: {}",
        body
    );
    // no <Status> element when disabled
    assert!(!body.contains("<Status>Enabled</Status>"));
    assert!(!body.contains("<Status>Suspended</Status>"));
}

#[tokio::test]
async fn versioning_enable_and_get() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    let resp = client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, "/tbk?versioning"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("<Status>Enabled</Status>"), "body: {}", body);
}

#[tokio::test]
async fn versioning_enable_then_suspend() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    let xml = r#"<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>"#;
    let resp = client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, "/tbk?versioning"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Status>Suspended</Status>"),
        "body: {}",
        body
    );
}

#[tokio::test]
async fn versioning_invalid_status_returns_400() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Invalid</Status></VersioningConfiguration>"#;
    let resp = client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

// --- Versioned PUT creates versions ---

#[tokio::test]
async fn versioned_put_returns_version_id() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // enable versioning
    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    // PUT should return x-amz-version-id
    let resp = client
        .put(url(&addr, "/tbk/obj"))
        .body("v1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().contains_key("x-amz-version-id"),
        "missing x-amz-version-id header"
    );
    let vid1 = resp.headers()["x-amz-version-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(!vid1.is_empty());

    // second PUT should return different version id
    let resp = client
        .put(url(&addr, "/tbk/obj"))
        .body("v2")
        .send()
        .await
        .unwrap();
    let vid2 = resp.headers()["x-amz-version-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(vid1, vid2);
}

// --- ListObjectVersions ---

#[tokio::test]
async fn list_object_versions_returns_all_versions() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    client
        .put(url(&addr, "/tbk/obj"))
        .body("v1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("v2")
        .send()
        .await
        .unwrap();

    let resp = client.get(url(&addr, "/tbk?versions")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("ListVersionsResult"),
        "expected ListVersionsResult: {}",
        body
    );
    // should contain at least 2 <Version> elements
    let version_count = body.matches("<Version>").count();
    assert!(
        version_count >= 2,
        "expected >= 2 versions, got {}: {}",
        version_count,
        body
    );
    assert!(body.contains("<IsLatest>true</IsLatest>"));
    assert!(body.contains("<VersionId>"));
}

#[tokio::test]
async fn list_object_versions_empty_bucket() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client.get(url(&addr, "/tbk?versions")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("ListVersionsResult"));
    assert!(!body.contains("<Version>"));
}

// --- Versioned DELETE creates delete marker ---

#[tokio::test]
async fn versioned_delete_creates_delete_marker() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    // delete without versionId -> should create delete marker
    let resp = client.delete(url(&addr, "/tbk/obj")).send().await.unwrap();
    assert_eq!(resp.status(), 204);
    assert!(resp.headers().contains_key("x-amz-delete-marker"));
    assert_eq!(resp.headers()["x-amz-delete-marker"], "true");
    assert!(resp.headers().contains_key("x-amz-version-id"));

    // list versions should show the delete marker
    let resp = client.get(url(&addr, "/tbk?versions")).send().await.unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<DeleteMarker>"),
        "expected DeleteMarker: {}",
        body
    );
}

// --- GET/DELETE with versionId ---

#[tokio::test]
async fn get_specific_version() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    let resp = client
        .put(url(&addr, "/tbk/obj"))
        .body("version-one")
        .send()
        .await
        .unwrap();
    let vid1 = resp.headers()["x-amz-version-id"]
        .to_str()
        .unwrap()
        .to_string();

    client
        .put(url(&addr, "/tbk/obj"))
        .body("version-two")
        .send()
        .await
        .unwrap();

    // GET without versionId -> latest (version-two)
    let resp = client.get(url(&addr, "/tbk/obj")).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "version-two");

    // GET with versionId=vid1 -> first version
    let resp = client
        .get(url_with_query(&addr, "/tbk/obj", &[("versionId", &vid1)]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "version-one");
}

#[tokio::test]
async fn delete_specific_version() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    let resp = client
        .put(url(&addr, "/tbk/obj"))
        .body("v1")
        .send()
        .await
        .unwrap();
    let vid1 = resp.headers()["x-amz-version-id"]
        .to_str()
        .unwrap()
        .to_string();

    client
        .put(url(&addr, "/tbk/obj"))
        .body("v2")
        .send()
        .await
        .unwrap();

    // permanently delete v1
    let resp = client
        .delete(url_with_query(&addr, "/tbk/obj", &[("versionId", &vid1)]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(resp.headers()["x-amz-version-id"], vid1.as_str());

    // GET v1 should now fail
    let resp = client
        .get(url_with_query(&addr, "/tbk/obj", &[("versionId", &vid1)]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // latest (v2) should still work
    let resp = client.get(url(&addr, "/tbk/obj")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "v2");
}

// --- Suspended versioning uses null version ---

#[tokio::test]
async fn suspended_versioning_uses_null_version() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // enable then suspend
    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    let xml = r#"<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tbk?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    // PUT should return version-id "null"
    let resp = client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-amz-version-id"], "null");
}

