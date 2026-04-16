//! Bucket policy enforcement through the S3 request path. Uses a
//! configured non-empty owner access key so policy evaluation runs
//! for non-owner callers, verifying that public-read grants
//! anonymous GET while explicit Deny shadows Allow.

use std::net::SocketAddr;
use std::sync::Arc;

use abixio::cluster::{ClusterConfig, ClusterManager};
use abixio::s3_route::AbixioDispatch;
use abixio::storage::Backend;
use abixio::storage::local_volume::LocalVolume;
use abixio::storage::metadata::BucketSettings;
use abixio::storage::volume_pool::VolumePool;
use abixio::storage::Store;
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

fn setup() -> (TempDir, Vec<std::path::PathBuf>) {
    let base = TempDir::new().unwrap();
    let mut paths = Vec::new();
    for i in 0..4 {
        let p = base.path().join(format!("d{}", i));
        std::fs::create_dir_all(&p).unwrap();
        paths.push(p);
    }
    (base, paths)
}

async fn start_server_with_owner(
    paths: &[std::path::PathBuf],
    owner_key: &str,
) -> (SocketAddr, Arc<VolumePool>) {
    let backends: Vec<Box<dyn Backend>> = paths
        .iter()
        .map(|p| Box::new(LocalVolume::new(p.as_path()).unwrap()) as Box<dyn Backend>)
        .collect();
    let set = Arc::new(VolumePool::new(backends).unwrap());

    let cluster = Arc::new(
        ClusterManager::new(ClusterConfig {
            node_id: "node-a".to_string(),
            advertise_s3: "http://127.0.0.1:0".to_string(),
            advertise_cluster: "http://127.0.0.1:0".to_string(),
            nodes: Vec::new(),
            access_key: String::new(),
            secret_key: String::new(),
            no_auth: true,
            disk_paths: paths.iter().cloned().collect(),
        })
        .unwrap(),
    );

    let s3 = abixio::s3_service::AbixioS3::new(Arc::clone(&set), Arc::clone(&cluster));
    let mut builder = s3s::service::S3ServiceBuilder::new(s3);
    builder.set_auth(AlwaysAllowAuth);
    builder.set_validation(abixio::s3_service::RelaxedNameValidation);
    builder.set_access(abixio::s3_access::AbixioAccess::new(
        Arc::clone(&cluster),
        Arc::clone(&set),
        owner_key.to_string(),
    ));
    let s3_service = builder.build();
    let dispatch = Arc::new(AbixioDispatch::new(s3_service, None, None));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
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
    (addr, set)
}

async fn attach_policy(set: &VolumePool, bucket: &str, policy: serde_json::Value) {
    let mut settings = set.get_bucket_settings(bucket).await.unwrap_or_default();
    settings.policy = Some(policy);
    set.set_bucket_settings(bucket, &settings).await.unwrap();
}

#[tokio::test]
async fn anon_get_allowed_on_public_read_bucket() {
    let (_base, paths) = setup();
    let (addr, set) = start_server_with_owner(&paths, "root-key").await;
    // Create bucket + object using the owner path (bucket policy does
    // not affect owner).
    let client = reqwest::Client::new();
    client.put(format!("http://{}/pub", addr)).send().await.unwrap();
    let put = client
        .put(format!("http://{}/pub/index.html", addr))
        .body("hello")
        .send().await.unwrap();
    assert_eq!(put.status(), 200);

    attach_policy(
        &set,
        "pub",
        serde_json::json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Principal": "*",
                "Action": "s3:GetObject",
                "Resource": "arn:aws:s3:::pub/*"
            }]
        }),
    ).await;

    // Anonymous GET should now succeed under the public-read policy.
    let resp = client.get(format!("http://{}/pub/index.html", addr)).send().await.unwrap();
    assert_eq!(resp.status(), 200, "anon GET should be allowed on public-read bucket");

    // Anonymous PUT should still be denied.
    let resp = client
        .put(format!("http://{}/pub/bad.html", addr))
        .body("nope")
        .send().await.unwrap();
    assert_eq!(resp.status(), 403, "anon PUT should be denied");
}

#[tokio::test]
async fn explicit_deny_overrides_allow_on_shared_bucket() {
    let (_base, paths) = setup();
    let (addr, set) = start_server_with_owner(&paths, "root-key").await;
    let client = reqwest::Client::new();
    client.put(format!("http://{}/b", addr)).send().await.unwrap();
    client
        .put(format!("http://{}/b/free/x", addr))
        .body("free")
        .send().await.unwrap();
    client
        .put(format!("http://{}/b/locked/x", addr))
        .body("locked")
        .send().await.unwrap();

    attach_policy(
        &set,
        "b",
        serde_json::json!({
            "Version": "2012-10-17",
            "Statement": [
                {"Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::b/*"},
                {"Effect": "Deny", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::b/locked/*"}
            ]
        }),
    ).await;

    let free = client.get(format!("http://{}/b/free/x", addr)).send().await.unwrap();
    assert_eq!(free.status(), 200);
    let locked = client.get(format!("http://{}/b/locked/x", addr)).send().await.unwrap();
    assert_eq!(locked.status(), 403, "explicit Deny should block this");
}
