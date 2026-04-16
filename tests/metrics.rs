//! Integration test for the Prometheus metrics endpoint.

use std::net::SocketAddr;
use std::sync::Arc;

use abixio::admin::HealStats;
use abixio::admin::handlers::{AdminConfig, AdminHandler};
use abixio::cluster::{ClusterConfig, ClusterManager};
use abixio::heal::mrf::MrfQueue;
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

fn setup() -> (TempDir, Vec<std::path::PathBuf>) {
    // Use a single disk so the GET path hits the 1+0 mmap fast
    // path which populates the read cache. The EC decode path does
    // not populate the cache today (future work).
    let base = TempDir::new().unwrap();
    let p = base.path().join("d0");
    std::fs::create_dir_all(&p).unwrap();
    (base, vec![p])
}

async fn start_server(paths: &[std::path::PathBuf]) -> SocketAddr {
    let backends: Vec<Box<dyn Backend>> = paths
        .iter()
        .map(|p| Box::new(LocalVolume::new(p.as_path()).unwrap()) as Box<dyn Backend>)
        .collect();
    let mut set = VolumePool::new(backends).unwrap();
    set.enable_read_cache(256 * 1024 * 1024, 64 * 1024);
    let metrics = abixio::metrics::Metrics::new();
    set.set_metrics(Arc::clone(&metrics));
    let set = Arc::new(set);

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
    let admin = Arc::new(
        AdminHandler::new(
            Arc::clone(&set),
            Arc::new(Vec::new()),
            Arc::new(MrfQueue::new(1000)),
            Arc::new(HealStats::new()),
            AdminConfig {
                listen: ":0".to_string(),
                total_disks: paths.len(),
                auth_enabled: false,
                scan_interval: "10m".to_string(),
                heal_interval: "24h".to_string(),
                mrf_workers: 2,
                node_id: "node-a".to_string(),
                advertise_s3: "http://127.0.0.1:0".to_string(),
                advertise_cluster: "http://127.0.0.1:0".to_string(),
                node_count: 1,
            },
            Arc::clone(&cluster),
        )
        .with_metrics(Arc::clone(&metrics)),
    );

    let s3 = abixio::s3_service::AbixioS3::new(Arc::clone(&set), Arc::clone(&cluster));
    let mut builder = s3s::service::S3ServiceBuilder::new(s3);
    builder.set_auth(AlwaysAllowAuth);
    builder.set_validation(abixio::s3_service::RelaxedNameValidation);
    builder.set_access(abixio::s3_access::AbixioAccess::new(
        Arc::clone(&cluster),
        Arc::clone(&set),
        String::new(),
    ));
    let s3_service = builder.build();
    let dispatch = Arc::new(
        AbixioDispatch::new(s3_service, Some(admin), None).with_metrics(Arc::clone(&metrics)),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = hyper_util::rt::TokioIo::new(stream);
            let d = dispatch.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req| {
                    let d = d.clone();
                    async move { Ok::<_, hyper::Error>(d.dispatch(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn metrics_endpoint_returns_expected_families() {
    let (_base, paths) = setup();
    let addr = start_server(&paths).await;
    let client = reqwest::Client::new();

    // exercise the request path so counters advance
    let resp = client.put(format!("http://{}/testb", addr)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client
        .put(format!("http://{}/testb/hello", addr))
        .body("hello world")
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client.get(format!("http://{}/testb/hello", addr)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    // second GET should hit the read cache
    let resp = client.get(format!("http://{}/testb/hello", addr)).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // scrape /metrics
    let scrape = client
        .get(format!("http://{}/_admin/metrics", addr))
        .send().await.unwrap();
    assert_eq!(scrape.status(), 200);
    let ct = scrape.headers().get("content-type").cloned();
    let body = scrape.text().await.unwrap();
    assert!(
        ct.as_ref()
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .starts_with("text/plain"),
        "unexpected content-type: {:?}",
        ct
    );

    // counter families present
    for name in [
        "abixio_s3_requests_total",
        "abixio_s3_request_duration_seconds",
        "abixio_disk_bytes_total",
        "abixio_read_cache_hits_total",
        "abixio_read_cache_misses_total",
        "abixio_cluster_state",
    ] {
        assert!(body.contains(name), "missing metric family {}: {}", name, body);
    }

    // PUT bucket + PUT object + GET x2 -> at least one success-class sample
    assert!(body.contains("status_class=\"ok\""), "no ok samples");

    // read cache should have at least one hit from the second GET
    let hits_line = body
        .lines()
        .find(|l| l.starts_with("abixio_read_cache_hits_total "))
        .expect("read cache hits line present");
    let hits: u64 = hits_line
        .split_whitespace()
        .last()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(hits >= 1, "expected >=1 read cache hits, got {}", hits);

    // disk capacity gauges should be present for every disk
    let disk_lines = body
        .lines()
        .filter(|l| l.starts_with("abixio_disk_bytes_total{"))
        .count();
    assert!(disk_lines >= paths.len(), "expected one disk line per disk");
}
