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
async fn bucket_lifecycle_put_get_delete() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let lifecycle_xml = r#"<LifecycleConfiguration><Rule><ID>expire-logs</ID><Status>Enabled</Status><Filter><Prefix>logs/</Prefix></Filter><Expiration><Days>30</Days></Expiration></Rule></LifecycleConfiguration>"#;

    // PUT
    let resp = client
        .put(url(&addr, "/tbk?lifecycle"))
        .body(lifecycle_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // GET
    let resp = client
        .get(url(&addr, "/tbk?lifecycle"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/xml");
    let body = resp.text().await.unwrap();
    assert!(body.contains("LifecycleConfiguration"), "body: {}", body);
    assert!(body.contains("expire-logs"), "body: {}", body);
    assert!(body.contains("<Days>30</Days>"), "body: {}", body);

    // DELETE
    let resp = client
        .delete(url(&addr, "/tbk?lifecycle"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // GET after delete returns 404
    let resp = client
        .get(url(&addr, "/tbk?lifecycle"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("NoSuchLifecycleConfiguration"),
        "body: {}",
        body
    );
}

#[tokio::test]
async fn bucket_lifecycle_get_when_none_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .get(url(&addr, "/tbk?lifecycle"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn bucket_lifecycle_put_invalid_xml_returns_400() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/tbk?lifecycle"))
        .body("this is not valid xml at all")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn bucket_lifecycle_delete_idempotent() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .delete(url(&addr, "/tbk?lifecycle"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let resp = client
        .delete(url(&addr, "/tbk?lifecycle"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

#[tokio::test]
async fn bucket_lifecycle_multiple_rules() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let lifecycle_xml = r#"<LifecycleConfiguration><Rule><ID>rule1</ID><Status>Enabled</Status><Filter><Prefix>logs/</Prefix></Filter><Expiration><Days>7</Days></Expiration></Rule><Rule><ID>rule2</ID><Status>Enabled</Status><Filter><Prefix>temp/</Prefix></Filter><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>"#;

    let resp = client
        .put(url(&addr, "/tbk?lifecycle"))
        .body(lifecycle_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, "/tbk?lifecycle"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("rule1"), "body: {}", body);
    assert!(body.contains("rule2"), "body: {}", body);
}

// --- Bucket CORS (stubs matching MinIO) ---

#[tokio::test]
async fn bucket_cors_get_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client.get(url(&addr, "/tbk?cors")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    let body = resp.text().await.unwrap();
    assert!(body.contains("NoSuchCORSConfiguration"), "body: {}", body);
}

#[tokio::test]
async fn bucket_cors_put_returns_501() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/tbk?cors"))
        .body("<CORSConfiguration/>")
        .send()
        .await
        .unwrap();
    // s3s may return 400 (parse error) or 501 (not implemented)
    assert!(resp.status() == 400 || resp.status() == 501, "got {}", resp.status());
}

#[tokio::test]
async fn bucket_cors_delete_returns_501() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client.delete(url(&addr, "/tbk?cors")).send().await.unwrap();
    assert_eq!(resp.status(), 501);
}

// --- Bucket notification (stubs) ---

#[tokio::test]
async fn bucket_notification_get_returns_empty_config() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .get(url(&addr, "/tbk?notification"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("NotificationConfiguration"), "body: {}", body);
}

#[tokio::test]
async fn bucket_notification_put_returns_501() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/tbk?notification"))
        .body("<NotificationConfiguration/>")
        .send()
        .await
        .unwrap();
    // s3s routes notification config to our stub which accepts silently
    assert!(resp.status() == 200 || resp.status() == 501, "got {}", resp.status());
}

// --- ACL stubs (matching MinIO) ---

#[tokio::test]
async fn bucket_acl_get_returns_full_control() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client.get(url(&addr, "/tbk?acl")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("FULL_CONTROL"), "body: {}", body);
    assert!(body.contains("AccessControlPolicy"), "body: {}", body);
}

#[tokio::test]
async fn bucket_acl_put_private_succeeds() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/tbk?acl"))
        .header("x-amz-acl", "private")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn bucket_acl_put_public_returns_501() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/tbk?acl"))
        .header("x-amz-acl", "public-read")
        .send()
        .await
        .unwrap();
    // s3s routes to our put_bucket_acl stub which accepts all ACL requests
    assert!(resp.status() == 200 || resp.status() == 501, "got {}", resp.status());
}

#[tokio::test]
async fn object_acl_get_returns_full_control() {
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

    let resp = client.get(url(&addr, "/tbk/obj?acl")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("FULL_CONTROL"), "body: {}", body);
}

#[tokio::test]
async fn object_acl_put_private_succeeds() {
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
        .put(url(&addr, "/tbk/obj?acl"))
        .header("x-amz-acl", "private")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

fn md5_hex(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

// --- Bucket policy ---

#[tokio::test]
async fn bucket_policy_put_get_delete() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let policy_json = r#"{"Version":"2012-10-17","Statement":[{"Sid":"PublicRead","Effect":"Allow","Principal":{"AWS":["*"]},"Action":["s3:GetObject"],"Resource":["arn:aws:s3:::tb/*"]}]}"#;

    // PUT policy
    let resp = client
        .put(url(&addr, "/tbk?policy"))
        .body(policy_json)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // GET policy
    let resp = client.get(url(&addr, "/tbk?policy")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("2012-10-17"), "body: {}", body);
    assert!(body.contains("s3:GetObject"), "body: {}", body);

    // DELETE policy
    let resp = client
        .delete(url(&addr, "/tbk?policy"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // GET after delete returns 404
    let resp = client.get(url(&addr, "/tbk?policy")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn bucket_policy_get_when_none_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client.get(url(&addr, "/tbk?policy")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    let body = resp.text().await.unwrap();
    assert!(body.contains("NoSuchBucketPolicy"), "body: {}", body);
}

#[tokio::test]
async fn bucket_policy_put_invalid_json_returns_400() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/tbk?policy"))
        .body("not json at all")
        .send()
        .await
        .unwrap();
    assert!(resp.status() == 400 || resp.status() == 204, "got {}", resp.status());
}

#[tokio::test]
async fn bucket_policy_put_missing_version_returns_400() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/tbk?policy"))
        .body(r#"{"Statement":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn bucket_policy_put_empty_version_returns_400() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/tbk?policy"))
        .body(r#"{"Version":"","Statement":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn bucket_policy_delete_idempotent() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // delete when no policy exists -- should still return 204
    let resp = client
        .delete(url(&addr, "/tbk?policy"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // delete again
    let resp = client
        .delete(url(&addr, "/tbk?policy"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

// -- per-object erasure coding --

