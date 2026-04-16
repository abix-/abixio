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
async fn object_tagging_put_get_delete() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    // get tags on fresh object -- should return empty TagSet
    let resp = client
        .get(url(&addr, "/tbk/obj?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Tagging"), "expected Tagging XML: {}", body);

    // put tags
    let tag_xml = r#"<Tagging><TagSet><Tag><Key>env</Key><Value>prod</Value></Tag><Tag><Key>team</Key><Value>infra</Value></Tag></TagSet></Tagging>"#;
    let resp = client
        .put(url(&addr, "/tbk/obj?tagging"))
        .body(tag_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // get tags back
    let resp = client
        .get(url(&addr, "/tbk/obj?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("env"), "missing tag key 'env': {}", body);
    assert!(body.contains("prod"), "missing tag value 'prod': {}", body);
    assert!(body.contains("team"), "missing tag key 'team': {}", body);
    assert!(
        body.contains("infra"),
        "missing tag value 'infra': {}",
        body
    );

    // delete tags
    let resp = client
        .delete(url(&addr, "/tbk/obj?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // verify tags are gone
    let resp = client
        .get(url(&addr, "/tbk/obj?tagging"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(!body.contains("env"), "tag should be gone: {}", body);
}

#[tokio::test]
async fn object_tagging_on_nonexistent_object_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .get(url(&addr, "/tbk/missing?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn object_tagging_malformed_xml_returns_400() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client
        .put(url(&addr, "/tbk/obj?tagging"))
        .body("not xml at all")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

// --- Bucket tagging ---

#[tokio::test]
async fn bucket_tagging_put_get_delete() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // get bucket tags (empty initially)
    let resp = client.get(url(&addr, "/tbk?tagging")).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // put bucket tags
    let tag_xml =
        r#"<Tagging><TagSet><Tag><Key>project</Key><Value>alpha</Value></Tag></TagSet></Tagging>"#;
    let resp = client
        .put(url(&addr, "/tbk?tagging"))
        .body(tag_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // get bucket tags back
    let resp = client.get(url(&addr, "/tbk?tagging")).send().await.unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("project"), "missing key: {}", body);
    assert!(body.contains("alpha"), "missing value: {}", body);

    // delete bucket tags
    let resp = client
        .delete(url(&addr, "/tbk?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

// --- Conditional requests ---

#[tokio::test]
async fn if_none_match_returns_304() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    let resp = client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();
    let etag = resp.headers()["etag"].to_str().unwrap().to_string();

    // GET with matching If-None-Match -> 304
    let resp = client
        .get(url(&addr, "/tbk/obj"))
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 304);

    // HEAD with matching If-None-Match -> 304
    let resp = client
        .head(url(&addr, "/tbk/obj"))
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 304);
}

#[tokio::test]
async fn if_none_match_different_etag_returns_200() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/tbk/obj"))
        .header("If-None-Match", "\"nonexistent-etag\"")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn if_match_returns_412_on_mismatch() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    // If-Match with wrong etag -> 412
    let resp = client
        .get(url(&addr, "/tbk/obj"))
        .header("If-Match", "\"wrong-etag\"")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 412);
}

#[tokio::test]
async fn if_match_correct_etag_returns_200() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    let resp = client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();
    let etag = resp.headers()["etag"].to_str().unwrap().to_string();

    let resp = client
        .get(url(&addr, "/tbk/obj"))
        .header("If-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn if_modified_since_returns_304_when_not_modified() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    // use a date far in the future
    let resp = client
        .get(url(&addr, "/tbk/obj"))
        .header("If-Modified-Since", "Sun, 01 Jan 2090 00:00:00 GMT")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 304);
}

#[tokio::test]
async fn if_unmodified_since_returns_412_when_modified() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    // use a date far in the past
    let resp = client
        .get(url(&addr, "/tbk/obj"))
        .header("If-Unmodified-Since", "Thu, 01 Jan 2000 00:00:00 GMT")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 412);
}

// --- Bucket versioning config ---

