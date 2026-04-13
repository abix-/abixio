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
async fn create_bucket() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client.put(url(&addr, "/testbucket")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn create_bucket_twice_returns_409() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    let resp = client.put(url(&addr, "/testbucket")).send().await.unwrap();
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn head_bucket_after_create() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    // before create
    let resp = client.head(url(&addr, "/testbucket")).send().await.unwrap();
    assert_eq!(resp.status(), 404);

    // create
    client.put(url(&addr, "/testbucket")).send().await.unwrap();

    // after create
    let resp = client.head(url(&addr, "/testbucket")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn list_buckets() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/alpha")).send().await.unwrap();
    client.put(url(&addr, "/beta")).send().await.unwrap();

    let resp = client.get(url(&addr, "/")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("alpha"));
    assert!(body.contains("beta"));
    assert!(body.contains("ListAllMyBucketsResult"));
}

#[tokio::test]
async fn get_nonexistent_bucket_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(url(&addr, "/nonexistent/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn delete_bucket_non_empty_returns_409() {
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

    let resp = client.delete(url(&addr, "/tbk")).send().await.unwrap();
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn delete_bucket_empty_succeeds() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client.delete(url(&addr, "/tbk")).send().await.unwrap();
    assert_eq!(resp.status(), 204);

    // bucket should be gone
    let resp = client.head(url(&addr, "/tbk")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

// --- Method not allowed ---

#[tokio::test]
async fn operations_on_missing_bucket_return_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    // GET object in nonexistent bucket
    let resp = client
        .get(url(&addr, "/nonexistent/obj"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // PUT object in nonexistent bucket
    let resp = client
        .put(url(&addr, "/nonexistent/obj"))
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // DELETE object in nonexistent bucket
    let resp = client
        .delete(url(&addr, "/nonexistent/obj"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // LIST objects in nonexistent bucket
    let resp = client
        .get(url_with_query(
            &addr,
            "/nonexistent",
            &[("list-type", "2")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// --- Empty body edge cases ---

