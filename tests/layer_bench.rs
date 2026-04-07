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

    // tokio::fs::write (page cache -- may not hit disk)
    let mut timings = Vec::new();
    for i in 0..iters {
        let path = tmp.path().join(format!("raw_{}", i));
        let t = Instant::now();
        tokio::fs::write(&path, &data).await.unwrap();
        timings.push(t.elapsed());
    }
    run_n("tokio::fs::write (cached)", size, iters, &mut timings);

    // tokio::fs::write + fsync (forces data to physical disk)
    let mut timings = Vec::new();
    for i in 0..iters {
        let path = tmp.path().join(format!("sync_{}", i));
        let t = Instant::now();
        {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::File::create(&path).await.unwrap();
            file.write_all(&data).await.unwrap();
            file.sync_all().await.unwrap();
        }
        timings.push(t.elapsed());
    }
    run_n("tokio::fs::write + fsync (disk)", size, iters, &mut timings);

    // tokio::fs::read (will be cached from writes above)
    let mut timings = Vec::new();
    for i in 0..iters {
        let path = tmp.path().join(format!("raw_{}", i));
        let t = Instant::now();
        let _ = tokio::fs::read(&path).await.unwrap();
        timings.push(t.elapsed());
    }
    run_n("tokio::fs::read (cached)", size, iters, &mut timings);

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

        // streaming PUT (simulates HTTP body as chunk stream)
        let mut timings = Vec::new();
        for i in 0..iters {
            let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = data
                .chunks(64 * 1024) // 64KB chunks like TCP
                .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
                .collect();
            let stream = futures::stream::iter(chunks);
            let t = Instant::now();
            pool.put_object_stream("bench", &format!("s/{i}"), stream, opts(), None).await.unwrap();
            timings.push(t.elapsed());
        }
        run_n(&format!("VolumePool::put_object_stream ({disks} disk)"), size, iters, &mut timings);

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

    let iters = 30;

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

    // raw reqwest GET (server returns 10MB body)
    // start a server that returns a 10MB response
    let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();
    let response_data = bytes::Bytes::from(data.clone());

    tokio::spawn(async move {
        loop {
            let (stream, _) = listener2.accept().await.unwrap();
            let io = hyper_util::rt::TokioIo::new(stream);
            let body = response_data.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |_req: hyper::Request<hyper::body::Incoming>| {
                    let body = body.clone();
                    async move {
                        Ok::<_, hyper::Error>(hyper::Response::new(
                            http_body_util::Full::new(body),
                        ))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    let url2 = format!("http://{}/test", addr2);
    // warmup
    for _ in 0..3 {
        let r = client.get(&url2).send().await.unwrap();
        let _ = r.bytes().await.unwrap();
    }

    let mut timings = Vec::new();
    for _ in 0..iters {
        let t = Instant::now();
        let r = client.get(&url2).send().await.unwrap();
        let _ = r.bytes().await.unwrap();
        timings.push(t.elapsed());
    }
    run_n("reqwest GET <- hyper (10MB body)", size, iters, &mut timings);
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

// ============================================================================
// bench_perf: structured multi-size benchmark with JSON output and comparison
// ============================================================================
//
// Usage:
//   # establish baseline
//   cargo test --release --test layer_bench -- --ignored bench_perf --nocapture
//
//   # compare after a change
//   BENCH_COMPARE=bench-results/baseline.json cargo test --release --test layer_bench -- --ignored bench_perf --nocapture
//
//   # test specific sizes (bytes, comma-separated)
//   BENCH_SIZES=1048576,10485760 cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct BenchResult {
    layer: String,
    op: String,
    disks: usize,
    size: usize,
    iters: usize,
    avg_ms: f64,
    p50_ms: f64,
    mbps: f64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BenchReport {
    timestamp: String,
    git_commit: String,
    results: Vec<BenchResult>,
}

fn measure(
    layer: &str,
    op: &str,
    disks: usize,
    size: usize,
    iters: usize,
    timings: &mut Vec<Duration>,
) -> BenchResult {
    timings.sort();
    let total: Duration = timings.iter().sum();
    let avg = total / iters as u32;
    let p50 = timings[iters / 2];
    let mbps = (size * iters) as f64 / total.as_secs_f64() / MB as f64;
    BenchResult {
        layer: layer.to_string(),
        op: op.to_string(),
        disks,
        size,
        iters,
        avg_ms: avg.as_secs_f64() * 1000.0,
        p50_ms: p50.as_secs_f64() * 1000.0,
        mbps,
    }
}

fn iters_for_size(size: usize, base: usize) -> usize {
    if size <= 4096 { base * 5 }       // 4KB: 50 iters
    else if size >= 1024 * MB { base / 2 } // 1GB: 5 iters
    else { base }                       // 10MB: 10 iters
}

fn human(size: usize) -> String {
    if size >= 1024 * MB { format!("{}GB", size / (1024 * MB)) }
    else if size >= MB { format!("{}MB", size / MB) }
    else { format!("{}KB", size / 1024) }
}

fn git_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_perf() {
    let default_sizes = vec![4096, 10 * MB, 1024 * MB];
    let sizes: Vec<usize> = std::env::var("BENCH_SIZES")
        .ok()
        .map(|s| s.split(',').filter_map(|v| v.trim().parse().ok()).collect())
        .unwrap_or(default_sizes);

    let base_iters: usize = std::env::var("BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let layers: Vec<String> = std::env::var("BENCH_LAYERS")
        .ok()
        .map(|s| s.split(',').map(|v| v.trim().to_uppercase()).collect())
        .unwrap_or_else(|| vec!["L1".into(), "L2".into(), "L3".into(), "L4".into(), "L5".into(), "L6".into()]);

    let mut results: Vec<BenchResult> = Vec::new();
    let tmp = TempDir::new().unwrap();

    eprintln!("\n=== bench_perf: layers {:?}, sizes {:?} ===\n", layers, sizes.iter().map(|s| human(*s)).collect::<Vec<_>>());

    fn emit(r: &BenchResult) {
        eprintln!("  {:<3} {:<12} {:>2} disk  {:>5}  {:>8.1} MB/s  avg {:>8.2}ms  p50 {:>8.2}ms",
            r.layer, r.op, r.disks, human(r.size), r.mbps, r.avg_ms, r.p50_ms);
    }

    // ---- L1: Hashing ----
    if layers.contains(&"L1".to_string()) {
        eprintln!("--- L1: Hashing ---");
        for &size in &sizes {
            let data = vec![0x42u8; size];
            let iters = iters_for_size(size, base_iters);

            let mut timings = Vec::new();
            for _ in 0..iters {
                let t = Instant::now();
                let _ = abixio::storage::bitrot::blake3_hex(&data);
                timings.push(t.elapsed());
            }
            let r = measure("L1", "blake3", 0, size, iters, &mut timings);
            emit(&r); results.push(r);

            let mut timings = Vec::new();
            for _ in 0..iters {
                let t = Instant::now();
                let _ = abixio::storage::bitrot::md5_hex(&data);
                timings.push(t.elapsed());
            }
            let r = measure("L1", "md5", 0, size, iters, &mut timings);
            emit(&r); results.push(r);
        }
        eprintln!();
    }

    // ---- L2: RS encode ----
    if layers.contains(&"L2".to_string()) {
        eprintln!("--- L2: RS encode ---");
        let rs = reed_solomon_erasure::galois_8::ReedSolomon::new(3, 1).unwrap();
        for &size in &sizes {
            let data = vec![0x42u8; size];
            let iters = iters_for_size(size, base_iters);

            let mut timings = Vec::new();
            for _ in 0..iters {
                let mut shards = abixio::storage::erasure_encode::split_data(&data, 3);
                let shard_size = shards[0].len();
                shards.push(vec![0u8; shard_size]);
                let t = Instant::now();
                rs.encode(&mut shards).unwrap();
                timings.push(t.elapsed());
            }
            let r = measure("L2", "rs_encode_3+1", 4, size, iters, &mut timings);
            emit(&r); results.push(r);
        }
        eprintln!();
    }

    // ---- L3: Disk I/O ----
    if layers.contains(&"L3".to_string()) {
        eprintln!("--- L3: Disk I/O ---");
        for &size in &sizes {
            let data = vec![0x42u8; size];
            let iters = iters_for_size(size, base_iters);

            let mut timings = Vec::new();
            for i in 0..iters {
                let path = tmp.path().join(format!("l3_w_{}", i));
                let t = Instant::now();
                tokio::fs::write(&path, &data).await.unwrap();
                timings.push(t.elapsed());
            }
            let r = measure("L3", "disk_write", 1, size, iters, &mut timings);
            emit(&r); results.push(r);

            let mut timings = Vec::new();
            for i in 0..iters {
                let path = tmp.path().join(format!("l3_w_{}", i));
                let t = Instant::now();
                let _ = tokio::fs::read(&path).await.unwrap();
                timings.push(t.elapsed());
            }
            let r = measure("L3", "disk_read", 1, size, iters, &mut timings);
            emit(&r); results.push(r);
        }
        eprintln!();
    }

    // ---- L4: Storage pipeline (VolumePool) ----
    if layers.contains(&"L4".to_string()) {
        eprintln!("--- L4: Storage pipeline ---");
        for &disk_count in &[1, 4] {
            let (_base, paths) = setup(disk_count);
            let pool = make_pool(&paths);
            pool.make_bucket("bench").await.unwrap();

            for &size in &sizes {
                let data = vec![0x42u8; size];
                let label = human(size);
                let iters = iters_for_size(size, base_iters);

                // warmup
                for i in 0..2 {
                    pool.put_object("bench", &format!("w/{}/{}", label, i), &data, opts()).await.unwrap();
                }

                // streaming PUT
                let mut timings = Vec::new();
                for i in 0..iters {
                    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = data
                        .chunks(64 * 1024)
                        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
                        .collect();
                    let stream = futures::stream::iter(chunks);
                    let t = Instant::now();
                    pool.put_object_stream("bench", &format!("s/{}/{}", label, i), stream, opts(), None).await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("L4", "put_stream", disk_count, size, iters, &mut timings);
                emit(&r); results.push(r);

                // GET (buffered)
                let mut timings = Vec::new();
                for i in 0..iters {
                    let t = Instant::now();
                    let _ = pool.get_object("bench", &format!("s/{}/{}", label, i)).await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("L4", "get", disk_count, size, iters, &mut timings);
                emit(&r); results.push(r);

                // GET (streaming)
                let mut timings = Vec::new();
                for i in 0..iters {
                    use futures::StreamExt;
                    let t = Instant::now();
                    let (_info, stream) = pool.get_object_stream("bench", &format!("s/{}/{}", label, i)).await.unwrap();
                    let mut stream = std::pin::pin!(stream);
                    while let Some(chunk) = stream.next().await {
                        let _ = chunk.unwrap();
                    }
                    timings.push(t.elapsed());
                }
                let r = measure("L4", "get_stream", disk_count, size, iters, &mut timings);
                emit(&r); results.push(r);
            }
        }
        eprintln!();
    }

    // ---- L5: HTTP transport (hyper only, no S3, no storage) ----
    if layers.contains(&"L5".to_string()) {
        eprintln!("--- L5: HTTP transport ---");

        // PUT server: reads body, returns 200
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let put_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let io = hyper_util::rt::TokioIo::new(stream);
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
                        use http_body_util::BodyExt;
                        let _ = req.into_body().collect().await;
                        Ok::<_, hyper::Error>(hyper::Response::new(
                            http_body_util::Full::new(hyper::body::Bytes::from("ok")),
                        ))
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await;
                });
            }
        });

        let client = reqwest::Client::new();

        for &size in &sizes {
            let data = vec![0x42u8; size];
            let iters = iters_for_size(size, base_iters);

            // warmup
            for _ in 0..3 {
                client.put(&format!("http://{}/test", put_addr)).body(data.clone()).send().await.unwrap();
            }

            let mut timings = Vec::new();
            for _ in 0..iters {
                let t = Instant::now();
                client.put(&format!("http://{}/test", put_addr)).body(data.clone()).send().await.unwrap();
                timings.push(t.elapsed());
            }
            let r = measure("L5", "http_put", 0, size, iters, &mut timings);
            emit(&r); results.push(r);

            // GET: start a size-specific response server
            let response_bytes = bytes::Bytes::from(data.clone());
            let get_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let get_addr = get_listener.local_addr().unwrap();
            let rb = response_bytes.clone();
            tokio::spawn(async move {
                loop {
                    let (stream, _) = get_listener.accept().await.unwrap();
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let body = rb.clone();
                    tokio::spawn(async move {
                        let svc = hyper::service::service_fn(move |_req: hyper::Request<hyper::body::Incoming>| {
                            let body = body.clone();
                            async move {
                                Ok::<_, hyper::Error>(hyper::Response::new(
                                    http_body_util::Full::new(body),
                                ))
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await;
                    });
                }
            });

            // warmup
            for _ in 0..3 {
                let resp = client.get(&format!("http://{}/test", get_addr)).send().await.unwrap();
                let _ = resp.bytes().await.unwrap();
            }

            let mut timings = Vec::new();
            for _ in 0..iters {
                let t = Instant::now();
                let resp = client.get(&format!("http://{}/test", get_addr)).send().await.unwrap();
                let _ = resp.bytes().await.unwrap();
                timings.push(t.elapsed());
            }
            let r = measure("L5", "http_get", 0, size, iters, &mut timings);
            emit(&r); results.push(r);
        }
        eprintln!();
    }

    // ---- L6: S3 protocol (s3s + storage, no SigV4, raw reqwest) ----
    if layers.contains(&"L6".to_string()) {
        eprintln!("--- L6: S3 protocol ---");

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
                    let svc = hyper::service::service_fn(move |req| {
                        let d = d.clone();
                        async move { Ok::<_, hyper::Error>(d.dispatch(req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await;
                });
            }
        });

        let client = reqwest::Client::new();

        for &size in &sizes {
            let data = vec![0x42u8; size];
            let iters = iters_for_size(size, base_iters);
            let label = human(size);

            // warmup
            for i in 0..3 {
                let url = format!("http://{}/bench/l6w_{}", addr, i);
                client.put(&url).body(data.clone()).send().await.unwrap();
            }

            // PUT through s3s (no SigV4)
            let mut timings = Vec::new();
            for i in 0..iters {
                let url = format!("http://{}/bench/l6_{}_{}", addr, label, i);
                let t = Instant::now();
                let resp = client.put(&url).body(data.clone()).send().await.unwrap();
                assert!(resp.status().is_success(), "PUT failed: {}", resp.status());
                timings.push(t.elapsed());
            }
            let r = measure("L6", "s3s_put", 1, size, iters, &mut timings);
            emit(&r); results.push(r);

            // GET through s3s
            let mut timings = Vec::new();
            for i in 0..iters {
                let url = format!("http://{}/bench/l6_{}_{}", addr, label, i);
                let t = Instant::now();
                let resp = client.get(&url).send().await.unwrap();
                let _ = resp.bytes().await.unwrap();
                timings.push(t.elapsed());
            }
            let r = measure("L6", "s3s_get", 1, size, iters, &mut timings);
            emit(&r); results.push(r);
        }
        eprintln!();
    }

    // save results
    let timestamp = chrono_lite_now();
    let commit = git_commit();
    let report = BenchReport {
        timestamp: timestamp.clone(),
        git_commit: commit.clone(),
        results: results.clone(),
    };

    let filename = format!("bench-results/{}.json", timestamp.replace(':', "-"));
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        let _ = std::fs::write(&filename, &json);
        eprintln!("\n  saved: {}", filename);
    }

    // compare mode
    if let Ok(baseline_path) = std::env::var("BENCH_COMPARE") {
        if let Ok(baseline_json) = std::fs::read_to_string(&baseline_path) {
            if let Ok(baseline) = serde_json::from_str::<BenchReport>(&baseline_json) {
                eprintln!("\n  comparing against: {} ({})\n", baseline_path, baseline.git_commit);
                eprintln!("  {:<4} {:<12} {:>5} {:>6} {:>12} {:>12} {:>8}",
                    "Layer", "Op", "Disks", "Size", "Baseline", "Current", "Delta");
                eprintln!("  {}", "-".repeat(68));
                for cur in &results {
                    if let Some(base) = baseline.results.iter().find(|b|
                        b.layer == cur.layer && b.op == cur.op && b.disks == cur.disks && b.size == cur.size
                    ) {
                        let delta = (cur.mbps - base.mbps) / base.mbps * 100.0;
                        let flag = if delta < -5.0 { " <-- REGRESSION" }
                            else if delta > 5.0 { " <-- FASTER" }
                            else { "" };
                        eprintln!("  {:<4} {:<12} {:>5} {:>6} {:>9.1} MB/s {:>9.1} MB/s {:>+7.1}%{}",
                            cur.layer, cur.op, cur.disks, human(cur.size),
                            base.mbps, cur.mbps, delta, flag);
                    }
                }
            }
        }
    }

    eprintln!();
}

fn chrono_lite_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    // rough UTC timestamp without chrono dependency
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    // days since epoch -> y/m/d (simplified, good enough for filenames)
    let mut y = 1970u64;
    let mut remaining_days = days;
    loop {
        let ydays = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
        if remaining_days < ydays { break; }
        remaining_days -= ydays;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays = [31, if leap {29} else {28}, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0u64;
    for md in mdays {
        if remaining_days < md { break; }
        remaining_days -= md;
        mo += 1;
    }
    format!("{:04}-{:02}-{:02}T{:02}-{:02}-{:02}Z", y, mo + 1, remaining_days + 1, h, m, s)
}
