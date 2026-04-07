//! Layer-by-layer isolation benchmarks.
//!
//! Measures each layer of the PUT path independently to identify
//! exactly where time is spent. Run with:
//!
//!   cargo test --release --test layer_bench -- --ignored --nocapture

use std::sync::Arc;
use std::time::{Duration, Instant};

use abixio::cluster::{ClusterConfig, ClusterManager};
use abixio::s3_route::AbixioDispatch;
use abixio::storage::local_volume::LocalVolume;
use abixio::storage::metadata::PutOptions;
use abixio::storage::volume_pool::VolumePool;
use abixio::storage::{Backend, Store};
use tempfile::TempDir;

const MB: usize = 1024 * 1024;

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

fn make_pool(paths: &[std::path::PathBuf]) -> VolumePool {
    let backends: Vec<Box<dyn Backend>> = paths
        .iter()
        .map(|p| Box::new(LocalVolume::new(p.as_path()).unwrap()) as Box<dyn Backend>)
        .collect();
    VolumePool::new(backends).unwrap()
}

fn opts() -> PutOptions {
    PutOptions {
        content_type: "application/octet-stream".to_string(),
        ..Default::default()
    }
}

fn run_n(name: &str, size: usize, iters: usize, timings: &mut Vec<Duration>) {
    timings.sort();
    let total: Duration = timings.iter().sum();
    let avg = total / iters as u32;
    let p50 = timings[iters / 2];
    let tp = (size * iters) as f64 / total.as_secs_f64() / MB as f64;
    eprintln!(
        "  {:<40} {:>6} ops  avg {:>8.2}ms  p50 {:>8.2}ms  {:>8.1} MB/s",
        name, iters,
        avg.as_secs_f64() * 1000.0,
        p50.as_secs_f64() * 1000.0,
        tp,
    );
}

// ============================================================================
// Layer 1: Raw primitives
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_layer_1_primitives() {
    let size = 10 * MB;
    let data = vec![0x42u8; size];
    let tmp = TempDir::new().unwrap();
    let iters = 20;

    eprintln!("\n=== Layer 1: Raw primitives (10MB) ===\n");

    // tokio::fs::write
    let mut timings = Vec::new();
    for i in 0..iters {
        let path = tmp.path().join(format!("raw_{}", i));
        let t = Instant::now();
        tokio::fs::write(&path, &data).await.unwrap();
        timings.push(t.elapsed());
    }
    run_n("tokio::fs::write", size, iters, &mut timings);

    // tokio::fs::read
    let mut timings = Vec::new();
    for i in 0..iters {
        let path = tmp.path().join(format!("raw_{}", i));
        let t = Instant::now();
        let _ = tokio::fs::read(&path).await.unwrap();
        timings.push(t.elapsed());
    }
    run_n("tokio::fs::read", size, iters, &mut timings);

    // MD5
    let mut timings = Vec::new();
    for _ in 0..iters {
        let t = Instant::now();
        let _ = abixio::storage::bitrot::md5_hex(&data);
        timings.push(t.elapsed());
    }
    run_n("md5_hex", size, iters, &mut timings);

    // blake3
    let mut timings = Vec::new();
    for _ in 0..iters {
        let t = Instant::now();
        let _ = abixio::storage::bitrot::blake3_hex(&data);
        timings.push(t.elapsed());
    }
    run_n("blake3_hex", size, iters, &mut timings);

    // SHA256 (for reference)
    let mut timings = Vec::new();
    for _ in 0..iters {
        let t = Instant::now();
        let _ = abixio::storage::bitrot::sha256_hex(&data);
        timings.push(t.elapsed());
    }
    run_n("sha256_hex (reference)", size, iters, &mut timings);

    // reed-solomon encode (3+1)
    let mut timings = Vec::new();
    let rs = reed_solomon_erasure::galois_8::ReedSolomon::new(3, 1).unwrap();
    for _ in 0..iters {
        let mut shards = abixio::storage::erasure_encode::split_data(&data, 3);
        let shard_size = shards[0].len();
        shards.push(vec![0u8; shard_size]);
        let t = Instant::now();
        rs.encode(&mut shards).unwrap();
        timings.push(t.elapsed());
    }
    run_n("reed-solomon encode 3+1", size, iters, &mut timings);
}

// ============================================================================
// Layer 2: Storage layer (VolumePool, no HTTP)
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_layer_2_storage() {
    let size = 10 * MB;
    let data = vec![0x42u8; size];
    let iters = 10;

    eprintln!("\n=== Layer 2: VolumePool direct (10MB) ===\n");

    for disks in [1, 4] {
        let (_base, paths) = setup(disks);
        let pool = make_pool(&paths);
        pool.make_bucket("bench").await.unwrap();

        // warmup
        for i in 0..3 {
            pool.put_object("bench", &format!("w/{i}"), &data, opts()).await.unwrap();
        }

        // PUT
        let mut timings = Vec::new();
        for i in 0..iters {
            let t = Instant::now();
            pool.put_object("bench", &format!("p/{i}"), &data, opts()).await.unwrap();
            timings.push(t.elapsed());
        }
        run_n(&format!("VolumePool::put_object ({disks} disk)"), size, iters, &mut timings);

        // GET
        let mut timings = Vec::new();
        for i in 0..iters {
            let t = Instant::now();
            let _ = pool.get_object("bench", &format!("p/{i}")).await.unwrap();
            timings.push(t.elapsed());
        }
        run_n(&format!("VolumePool::get_object ({disks} disk)"), size, iters, &mut timings);
    }
}

// ============================================================================
// Layer 3: HTTP transport (no S3, no SigV4)
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_layer_3_http_transport() {
    let size = 10 * MB;
    let data = vec![0x42u8; size];
    let iters = 10;

    eprintln!("\n=== Layer 3: HTTP transport only (10MB) ===\n");

    // start a minimal hyper server that just reads the body and returns 200
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = hyper_util::rt::TokioIo::new(stream);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
                    // consume the body fully
                    use http_body_util::BodyExt;
                    let _ = req.into_body().collect().await;
                    Ok::<_, hyper::Error>(hyper::Response::new(
                        http_body_util::Full::new(hyper::body::Bytes::from("ok")),
                    ))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    // raw reqwest PUT (no SigV4, just HTTP)
    let client = reqwest::Client::new();
    let url = format!("http://{}/test", addr);

    // warmup
    for _ in 0..3 {
        client.put(&url).body(data.clone()).send().await.unwrap();
    }

    let mut timings = Vec::new();
    for _ in 0..iters {
        let t = Instant::now();
        client.put(&url).body(data.clone()).send().await.unwrap();
        timings.push(t.elapsed());
    }
    run_n("reqwest PUT -> hyper (body discard)", size, iters, &mut timings);
}

// ============================================================================
// Layer 4: s3s protocol (SigV4 + XML, no storage)
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_layer_4_s3s_protocol() {
    let size = 10 * MB;
    let data = vec![0x42u8; size];
    let iters = 10;

    eprintln!("\n=== Layer 4: s3s protocol overhead (10MB) ===\n");

    // start abixio server with 1 disk
    let (_base, paths) = setup(1);
    let pool = Arc::new(make_pool(&paths));
    pool.make_bucket("bench").await.unwrap();

    let cluster = Arc::new(
        ClusterManager::new(ClusterConfig {
            node_id: "test".to_string(),
            advertise_s3: "http://127.0.0.1:0".to_string(),
            advertise_cluster: "http://127.0.0.1:0".to_string(),
            nodes: Vec::new(),
            access_key: String::new(),
            secret_key: String::new(),
            no_auth: true,
            disk_paths: paths.clone(),
        })
        .unwrap(),
    );

    let s3 = abixio::s3_service::AbixioS3::new(Arc::clone(&pool), Arc::clone(&cluster));
    let mut builder = s3s::service::S3ServiceBuilder::new(s3);
    builder.set_validation(abixio::s3_service::RelaxedNameValidation);
    let s3_service = builder.build();
    let dispatch = Arc::new(AbixioDispatch::new(s3_service, None, None));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dispatch_clone = dispatch.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = hyper_util::rt::TokioIo::new(stream);
            let d = dispatch_clone.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let d = d.clone();
                    async move { Ok::<_, hyper::Error>(d.dispatch(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    // reqwest PUT with SigV4 (via aws-sdk would be ideal, but reqwest is simpler)
    // use raw reqwest to isolate server-side overhead from client SigV4
    let client = reqwest::Client::new();

    // warmup
    for i in 0..3 {
        let url = format!("http://{}/bench/warmup{}", addr, i);
        client.put(&url).body(data.clone()).send().await.unwrap();
    }

    // unsigned PUT through s3s (tests s3s parsing + storage)
    let mut timings = Vec::new();
    for i in 0..iters {
        let url = format!("http://{}/bench/s3s_{}", addr, i);
        let t = Instant::now();
        let resp = client.put(&url).body(data.clone()).send().await.unwrap();
        timings.push(t.elapsed());
        assert!(resp.status().is_success(), "PUT failed: {}", resp.status());
    }
    run_n("reqwest PUT -> s3s -> VolumePool (1 disk)", size, iters, &mut timings);

    // GET back through s3s
    let mut timings = Vec::new();
    for i in 0..iters {
        let url = format!("http://{}/bench/s3s_{}", addr, i);
        let t = Instant::now();
        let resp = client.get(&url).send().await.unwrap();
        let _ = resp.bytes().await.unwrap();
        timings.push(t.elapsed());
    }
    run_n("reqwest GET -> s3s -> VolumePool (1 disk)", size, iters, &mut timings);
}

// ============================================================================
// Layer 5: Full client (aws-sdk-s3 with SigV4)
// ============================================================================

// This is what abixio-ui/tests/bench.rs measures.
// We include it here for the side-by-side comparison.

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_layer_5_full_client() {
    eprintln!("\n=== Layer 5: Full aws-sdk-s3 client ===\n");
    eprintln!("  (run abixio-ui/tests/bench.rs for this layer)");
    eprintln!("  Latest: PUT 10MB 1 disk = ~14 MB/s, GET 10MB = ~291 MB/s");
}
