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
            pool.put_object_stream("bench", &format!("s/{i}"), stream, opts(), None, None).await.unwrap();
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
            stream.set_nodelay(true).ok();
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
            stream.set_nodelay(true).ok();
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
            stream.set_nodelay(true).ok();
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
// Pool L0: WriteSlotPool primitive in isolation (no I/O orchestration)
// ============================================================================
//
// Phase 1 of the write-pool work (see docs/write-pool.md). Measures the
// pool primitive WITHOUT any of the integration: no rename worker, no
// pending_renames, no actual writes to slot files. Just pop+release
// cycles, init time, and contention behavior.
//
// Run with:
//   k3sc cargo-lock test --release --test layer_bench -- --ignored \
//       --nocapture bench_pool_l0_primitive

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_pool_l0_primitive() {
    use abixio::storage::write_slot_pool::WriteSlotPool;

    let tmp = TempDir::new().unwrap();
    let pool_dir = tmp.path().join("preopen");
    let depth = 32u32;

    eprintln!("\n=== Pool L0: WriteSlotPool primitive ===\n");

    // 1. Pool init time
    let t = Instant::now();
    let pool = WriteSlotPool::new(&pool_dir, depth).await.unwrap();
    let init_elapsed = t.elapsed();
    eprintln!(
        "  pool init (depth={}): {:.2}ms  ({:.2}us per slot pair)",
        depth,
        init_elapsed.as_secs_f64() * 1000.0,
        init_elapsed.as_secs_f64() * 1_000_000.0 / depth as f64,
    );
    assert_eq!(pool.available(), depth as usize);

    // 2. Single-thread pop+release latency (warmup, then measure)
    let warmup = 10_000;
    for _ in 0..warmup {
        let s = pool.try_pop().unwrap();
        pool.release(s).unwrap();
    }
    let iters = 100_000;
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let s = pool.try_pop().unwrap();
        pool.release(s).unwrap();
        samples.push(t.elapsed());
    }
    samples.sort();
    let total_ns: u128 = samples.iter().map(|d| d.as_nanos()).sum();
    let avg_ns = total_ns / iters as u128;
    let p50_ns = samples[iters / 2].as_nanos();
    let p99_ns = samples[(iters * 99) / 100].as_nanos();
    let p999_ns = samples[(iters * 999) / 1000].as_nanos();
    eprintln!(
        "  single-thread pop+release ({} iters):  avg {}ns  p50 {}ns  p99 {}ns  p999 {}ns",
        iters, avg_ns, p50_ns, p99_ns, p999_ns,
    );

    // 3. Concurrent pop+release throughput (bounded by pool depth)
    for workers in [2usize, 8, 32] {
        let per_worker = 10_000;
        let pool_clone = Arc::clone(&pool);
        let t = Instant::now();
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let p = Arc::clone(&pool_clone);
            handles.push(tokio::spawn(async move {
                for _ in 0..per_worker {
                    // Spin if pool is momentarily empty -- the test is
                    // measuring contention, not starvation handling.
                    let slot = loop {
                        if let Some(s) = p.try_pop() {
                            break s;
                        }
                        std::hint::spin_loop();
                    };
                    p.release(slot).unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let elapsed = t.elapsed();
        let total_ops = workers * per_worker;
        let ops_per_sec = total_ops as f64 / elapsed.as_secs_f64();
        eprintln!(
            "  {:>2} workers x {} ops:  {:.2}ms total  {:>10.0} ops/sec  {}ns/op",
            workers,
            per_worker,
            elapsed.as_secs_f64() * 1000.0,
            ops_per_sec,
            (elapsed.as_nanos() as u128) / total_ops as u128,
        );
    }

    // 4. Starvation behavior: drain the pool, verify try_pop returns None
    let mut held = Vec::with_capacity(depth as usize);
    for _ in 0..depth {
        held.push(pool.try_pop().unwrap());
    }
    assert_eq!(pool.available(), 0);
    let t = Instant::now();
    let drained = pool.try_pop();
    let drained_elapsed = t.elapsed();
    assert!(drained.is_none(), "try_pop on empty pool must return None");
    eprintln!(
        "  empty try_pop:  {}ns  (must return None without blocking)",
        drained_elapsed.as_nanos()
    );

    // restore the pool so the test cleanup is graceful
    for s in held {
        pool.release(s).unwrap();
    }
    assert_eq!(pool.available(), depth as usize);
}

// ============================================================================
// Pool L1: slot writes with real I/O (no orchestration)
// ============================================================================
//
// Phase 2 of the write-pool work. Measures the cost of writing shard
// bytes + meta JSON to a pre-opened slot, layered through six write
// strategies, vs the current file tier slow path. Still no rename
// worker, no pending_renames, no integration with LocalVolume. The
// goal is to attribute speedup to specific optimizations:
//   #1 concurrent writes via tokio::try_join!
//   #2 compact JSON instead of pretty
//   #3 sync std::fs::File::write_all for small payloads
//
// Run with:
//   k3sc cargo-lock test --release --test layer_bench -- --ignored \
//       --nocapture bench_pool_l1_slot_write

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_pool_l1_slot_write() {
    use abixio::storage::metadata::{ErasureMeta, ObjectMeta, ObjectMetaFile};
    use abixio::storage::write_slot_pool::{WriteSlot, WriteSlotPool};
    use std::collections::HashMap;
    use tokio::io::AsyncWriteExt;

    // Build a representative ObjectMetaFile and serialize both ways.
    fn make_test_meta() -> ObjectMetaFile {
        let meta = ObjectMeta {
            size: 4096,
            etag: "d41d8cd98f00b204e9800998ecf8427e".to_string(),
            content_type: "application/octet-stream".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                ftt: 1,
                index: 0,
                epoch_id: 1,
                volume_ids: vec![
                    "vol-0".to_string(),
                    "vol-1".to_string(),
                    "vol-2".to_string(),
                    "vol-3".to_string(),
                ],
            },
            checksum: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            user_metadata: HashMap::new(),
            tags: HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
            inline_data: None,
        };
        ObjectMetaFile {
            versions: vec![meta],
        }
    }

    fn fmt_dur(d: Duration) -> String {
        let us = d.as_secs_f64() * 1_000_000.0;
        if us >= 1000.0 {
            format!("{:.2}ms", us / 1000.0)
        } else {
            format!("{:.1}us", us)
        }
    }

    fn report_row(size: usize, label: &str, iters: usize, timings: &mut Vec<Duration>) {
        timings.sort();
        let total: Duration = timings.iter().sum();
        let avg = total / iters as u32;
        let p50 = timings[iters / 2];
        let p99_idx = ((iters * 99) / 100).min(iters - 1);
        let p99 = timings[p99_idx];
        let mbps = (size * iters) as f64 / total.as_secs_f64() / MB as f64;
        eprintln!(
            "  {:<8} {:<26} {:>5}  {:>9}  {:>9}  {:>9}  {:>9.1} MB/s",
            human(size),
            label,
            iters,
            fmt_dur(avg),
            fmt_dur(p50),
            fmt_dur(p99),
            mbps,
        );
    }

    let meta_file = make_test_meta();
    let meta_pretty: Vec<u8> = serde_json::to_vec_pretty(&meta_file).unwrap();
    let meta_compact: Vec<u8> = serde_json::to_vec(&meta_file).unwrap();

    eprintln!("\n=== Pool L1: slot write strategies ===\n");
    eprintln!(
        "  meta.json sizes:  pretty {} bytes,  compact {} bytes",
        meta_pretty.len(),
        meta_compact.len()
    );
    eprintln!();
    eprintln!(
        "  {:<8} {:<26} {:>5}  {:>9}  {:>9}  {:>9}  {:>14}",
        "SIZE", "STRATEGY", "ITERS", "AVG", "p50", "p99", "THROUGHPUT"
    );

    let sizes: &[(usize, usize)] = &[
        (4 * 1024, 100),
        (64 * 1024, 60),
        (1 * MB, 30),
        (10 * MB, 15),
        (100 * MB, 5),
    ];

    for &(size, iters) in sizes {
        let data = vec![0x42u8; size];
        let base = TempDir::new().unwrap();
        eprintln!();

        // ---------------------------------------------------------------
        // A: file_tier_full -- mkdir + 2 file creates per iter
        // ---------------------------------------------------------------
        let dir_a = base.path().join("a");
        std::fs::create_dir_all(&dir_a).unwrap();
        let mut timings = Vec::with_capacity(iters);
        for i in 0..iters {
            let obj_dir = dir_a.join(format!("obj_{}", i));
            let t = Instant::now();
            tokio::fs::create_dir_all(&obj_dir).await.unwrap();
            tokio::fs::write(obj_dir.join("shard.dat"), &data).await.unwrap();
            tokio::fs::write(obj_dir.join("meta.json"), &meta_pretty).await.unwrap();
            timings.push(t.elapsed());
        }
        report_row(size, "A: file_tier_full", iters, &mut timings);

        // ---------------------------------------------------------------
        // B: file_tier_no_mkdir -- pre-create dirs, only time the writes
        // ---------------------------------------------------------------
        let dir_b = base.path().join("b");
        std::fs::create_dir_all(&dir_b).unwrap();
        let obj_dirs_b: Vec<std::path::PathBuf> = (0..iters)
            .map(|i| {
                let p = dir_b.join(format!("obj_{}", i));
                std::fs::create_dir_all(&p).unwrap();
                p
            })
            .collect();
        let mut timings = Vec::with_capacity(iters);
        for obj_dir in &obj_dirs_b {
            let t = Instant::now();
            tokio::fs::write(obj_dir.join("shard.dat"), &data).await.unwrap();
            tokio::fs::write(obj_dir.join("meta.json"), &meta_pretty).await.unwrap();
            timings.push(t.elapsed());
        }
        report_row(size, "B: file_tier_no_mkdir", iters, &mut timings);

        // ---------------------------------------------------------------
        // C: pool_serial -- sequential async writes via WriteSlot
        // ---------------------------------------------------------------
        let dir_c = base.path().join("c");
        let pool = WriteSlotPool::new(&dir_c, iters as u32).await.unwrap();
        let mut timings = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let mut slot = pool.try_pop().unwrap();
            slot.data_file.write_all(&data).await.unwrap();
            slot.meta_file.write_all(&meta_pretty).await.unwrap();
            drop(slot);
            timings.push(t.elapsed());
        }
        report_row(size, "C: pool_serial", iters, &mut timings);

        // ---------------------------------------------------------------
        // D: pool_join -- concurrent writes via tokio::try_join! (#1)
        // ---------------------------------------------------------------
        let dir_d = base.path().join("d");
        let pool = WriteSlotPool::new(&dir_d, iters as u32).await.unwrap();
        let mut timings = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let WriteSlot {
                mut data_file,
                mut meta_file,
                ..
            } = pool.try_pop().unwrap();
            tokio::try_join!(
                data_file.write_all(&data),
                meta_file.write_all(&meta_pretty),
            )
            .unwrap();
            drop(data_file);
            drop(meta_file);
            timings.push(t.elapsed());
        }
        report_row(size, "D: pool_join", iters, &mut timings);

        // ---------------------------------------------------------------
        // E: pool_join_compact -- D + compact JSON (#1+#2)
        // ---------------------------------------------------------------
        let dir_e = base.path().join("e");
        let pool = WriteSlotPool::new(&dir_e, iters as u32).await.unwrap();
        let mut timings = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let WriteSlot {
                mut data_file,
                mut meta_file,
                ..
            } = pool.try_pop().unwrap();
            tokio::try_join!(
                data_file.write_all(&data),
                meta_file.write_all(&meta_compact),
            )
            .unwrap();
            drop(data_file);
            drop(meta_file);
            timings.push(t.elapsed());
        }
        report_row(size, "E: pool_join_compact", iters, &mut timings);

        // ---------------------------------------------------------------
        // F: pool_sync_small -- E + std::io sync writes (#1+#2+#3)
        //    Only meaningful for payloads <= 4KB. For larger sizes the
        //    sync write would block the tokio runtime too long.
        //    File handle conversion (into_std) happens OUTSIDE the timing.
        // ---------------------------------------------------------------
        if size <= 4 * 1024 {
            let dir_f = base.path().join("f");
            let pool = WriteSlotPool::new(&dir_f, iters as u32).await.unwrap();
            // Pre-convert all slot file handles to std::fs::File outside timing
            let mut std_pairs: Vec<(std::fs::File, std::fs::File)> = Vec::with_capacity(iters);
            for _ in 0..iters {
                let WriteSlot {
                    data_file,
                    meta_file,
                    ..
                } = pool.try_pop().unwrap();
                let data_std = data_file.into_std().await;
                let meta_std = meta_file.into_std().await;
                std_pairs.push((data_std, meta_std));
            }
            let mut timings = Vec::with_capacity(iters);
            for (mut data_std, mut meta_std) in std_pairs {
                use std::io::Write;
                let t = Instant::now();
                data_std.write_all(&data).unwrap();
                meta_std.write_all(&meta_compact).unwrap();
                timings.push(t.elapsed());
            }
            report_row(size, "F: pool_sync_small", iters, &mut timings);
        } else {
            eprintln!(
                "  {:<8} {:<26} {:>5}  {}",
                human(size),
                "F: pool_sync_small",
                "-",
                "(N/A above 4KB; would block runtime)"
            );
        }
    }

    eprintln!();
}

// ============================================================================
// Pool L1.5: JSON serializer comparison
// ============================================================================
//
// Phase 2.5 of the write-pool work. Phase 2 found that compact serde_json
// gave no measurable speed improvement over pretty serde_json, even though
// it produced ~35% smaller output. The user's stated goal is maximum JSON
// speed for the pool hot path. This bench measures three faster
// alternatives against serde_json on the exact same payload, with
// round-trip parse validation to confirm output compatibility.
//
// Run with:
//   k3sc cargo-lock test --release --test layer_bench -- --ignored \
//       --nocapture bench_pool_l1_5_json_serializers

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_pool_l1_5_json_serializers() {
    use abixio::storage::metadata::{ErasureMeta, ObjectMeta, ObjectMetaFile};
    use std::collections::HashMap;

    fn make_test_meta() -> ObjectMetaFile {
        let meta = ObjectMeta {
            size: 4096,
            etag: "d41d8cd98f00b204e9800998ecf8427e".to_string(),
            content_type: "application/octet-stream".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                ftt: 1,
                index: 0,
                epoch_id: 1,
                volume_ids: vec![
                    "vol-0".to_string(),
                    "vol-1".to_string(),
                    "vol-2".to_string(),
                    "vol-3".to_string(),
                ],
            },
            checksum: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            user_metadata: HashMap::new(),
            tags: HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
            inline_data: None,
        };
        ObjectMetaFile {
            versions: vec![meta],
        }
    }

    fn report(label: &str, iters: usize, mut samples: Vec<Duration>) {
        samples.sort();
        let total: Duration = samples.iter().sum();
        let avg = total / iters as u32;
        let p50 = samples[iters / 2];
        let p99 = samples[(iters * 99) / 100];
        let p999_idx = ((iters * 999) / 1000).min(iters - 1);
        let p999 = samples[p999_idx];
        eprintln!(
            "  {:<32}  avg {:>6}ns  p50 {:>6}ns  p99 {:>6}ns  p999 {:>6}ns",
            label,
            avg.as_nanos(),
            p50.as_nanos(),
            p99.as_nanos(),
            p999.as_nanos(),
        );
    }

    let meta = make_test_meta();
    let iters = 100_000;

    eprintln!("\n=== Pool L1.5: JSON serializer comparison ===\n");
    eprintln!("  payload: ObjectMetaFile with 1 version");
    eprintln!("  iterations: {}", iters);
    eprintln!();

    // sanity check + output sizes
    let json_serde_pretty = serde_json::to_vec_pretty(&meta).unwrap();
    let json_serde = serde_json::to_vec(&meta).unwrap();
    let json_simd = simd_json::serde::to_vec(&meta).unwrap();
    let json_sonic = sonic_rs::to_vec(&meta).unwrap();

    // round-trip parse via serde_json (the destination meta.json reader)
    for (label, bytes) in [
        ("serde_pretty", &json_serde_pretty),
        ("serde", &json_serde),
        ("simd-json", &json_simd),
        ("sonic-rs", &json_sonic),
    ] {
        let parsed: ObjectMetaFile = serde_json::from_slice(bytes).unwrap_or_else(|e| {
            panic!("{} produced JSON that serde_json cannot parse: {}", label, e)
        });
        assert_eq!(parsed, meta, "{} round-trip changed the struct", label);
    }

    eprintln!("  output sizes (bytes):");
    eprintln!("    A serde_json::to_vec_pretty:  {}", json_serde_pretty.len());
    eprintln!("    B serde_json::to_vec:         {}", json_serde.len());
    eprintln!("    C simd-json::serde::to_vec:   {}", json_simd.len());
    eprintln!("    D sonic-rs::to_vec:           {}", json_sonic.len());
    eprintln!();

    // warmup
    for _ in 0..10_000 {
        let _ = serde_json::to_vec(&meta).unwrap();
    }

    // A: serde_json::to_vec_pretty
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let _ = serde_json::to_vec_pretty(&meta).unwrap();
        samples.push(t.elapsed());
    }
    report("A: serde_json::to_vec_pretty", iters, samples);

    // B: serde_json::to_vec
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let _ = serde_json::to_vec(&meta).unwrap();
        samples.push(t.elapsed());
    }
    report("B: serde_json::to_vec", iters, samples);

    // C: simd-json::serde::to_vec
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let _ = simd_json::serde::to_vec(&meta).unwrap();
        samples.push(t.elapsed());
    }
    report("C: simd-json::serde::to_vec", iters, samples);

    // D: sonic-rs::to_vec
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let _ = sonic_rs::to_vec(&meta).unwrap();
        samples.push(t.elapsed());
    }
    report("D: sonic-rs::to_vec", iters, samples);

    eprintln!();
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
        .unwrap_or_else(|| vec!["L1".into(), "L2".into(), "L3".into(), "L4".into(), "L5".into(), "L5X".into(), "L6".into(), "EXP".into(), "L6A".into(), "L7".into()]);

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
                    pool.put_object_stream("bench", &format!("s/{}/{}", label, i), stream, opts(), None, None).await.unwrap();
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
                stream.set_nodelay(true).ok();
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
                    stream.set_nodelay(true).ok();
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

    // ---- L5X: HTTP transport sub-layer experiments ----
    // Isolates hyper config options and raw TCP to find where time goes.
    if layers.contains(&"L5X".to_string()) {
        eprintln!("--- L5X: HTTP transport experiments ---");

        let client = reqwest::Client::new();

        for &size in &sizes {
            if size < 1024 * 1024 { continue; }
            let data = vec![0x42u8; size];
            let iters = iters_for_size(size, base_iters);
            let label = human(size);

            // --- L5a: raw TCP (no HTTP) ---
            {
                let response_bytes = bytes::Bytes::from(data.clone());
                let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let tcp_addr = tcp_listener.local_addr().unwrap();
                let rb = response_bytes.clone();
                tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = tcp_listener.accept().await.unwrap();
                        stream.set_nodelay(true).ok();
                        let body = rb.clone();
                        tokio::spawn(async move {
                            use tokio::io::{AsyncReadExt, AsyncWriteExt};
                            let mut buf = [0u8; 256];
                            let _ = stream.read(&mut buf).await; // read request
                            let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(&body).await;
                            let _ = stream.flush().await;
                        });
                    }
                });

                // warmup
                for _ in 0..3 {
                    let resp = client.get(&format!("http://{}/test", tcp_addr)).send().await.unwrap();
                    let _ = resp.bytes().await.unwrap();
                }

                let mut timings = Vec::new();
                for _ in 0..iters {
                    let t = Instant::now();
                    let resp = client.get(&format!("http://{}/test", tcp_addr)).send().await.unwrap();
                    let _ = resp.bytes().await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("L5X", "raw_tcp_get", 0, size, iters, &mut timings);
                emit(&r); results.push(r);
            }

            // --- L5c: hyper writev(true) ---
            {
                let response_bytes = bytes::Bytes::from(data.clone());
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let rb = response_bytes.clone();
                tokio::spawn(async move {
                    loop {
                        let (stream, _) = listener.accept().await.unwrap();
                        stream.set_nodelay(true).ok();
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
                            let _ = hyper::server::conn::http1::Builder::new()
                                .writev(true)
                                .serve_connection(io, svc).await;
                        });
                    }
                });

                for _ in 0..3 {
                    let resp = client.get(&format!("http://{}/test", addr)).send().await.unwrap();
                    let _ = resp.bytes().await.unwrap();
                }

                let mut timings = Vec::new();
                for _ in 0..iters {
                    let t = Instant::now();
                    let resp = client.get(&format!("http://{}/test", addr)).send().await.unwrap();
                    let _ = resp.bytes().await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("L5X", "writev_get", 0, size, iters, &mut timings);
                emit(&r); results.push(r);
            }

            // --- L5d: hyper max_buf_size(4MB) ---
            {
                let response_bytes = bytes::Bytes::from(data.clone());
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let rb = response_bytes.clone();
                tokio::spawn(async move {
                    loop {
                        let (stream, _) = listener.accept().await.unwrap();
                        stream.set_nodelay(true).ok();
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
                            let _ = hyper::server::conn::http1::Builder::new()
                                .max_buf_size(4 * 1024 * 1024)
                                .serve_connection(io, svc).await;
                        });
                    }
                });

                for _ in 0..3 {
                    let resp = client.get(&format!("http://{}/test", addr)).send().await.unwrap();
                    let _ = resp.bytes().await.unwrap();
                }

                let mut timings = Vec::new();
                for _ in 0..iters {
                    let t = Instant::now();
                    let resp = client.get(&format!("http://{}/test", addr)).send().await.unwrap();
                    let _ = resp.bytes().await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("L5X", "bigbuf_get", 0, size, iters, &mut timings);
                emit(&r); results.push(r);
            }

            // --- L5e: hyper streamed body (4MB chunks) ---
            {
                let response_bytes = bytes::Bytes::from(data.clone());
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let rb = response_bytes.clone();
                tokio::spawn(async move {
                    loop {
                        let (stream, _) = listener.accept().await.unwrap();
                        stream.set_nodelay(true).ok();
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let body = rb.clone();
                        tokio::spawn(async move {
                            let svc = hyper::service::service_fn(move |_req: hyper::Request<hyper::body::Incoming>| {
                                let body = body.clone();
                                async move {
                                    // stream 4MB chunks
                                    let len = body.len();
                                    let chunk_size = 4 * 1024 * 1024;
                                    let chunks: Vec<Result<hyper::body::Frame<bytes::Bytes>, std::convert::Infallible>> =
                                        (0..len).step_by(chunk_size).map(|start| {
                                            let end = (start + chunk_size).min(len);
                                            Ok(hyper::body::Frame::data(body.slice(start..end)))
                                        }).collect();
                                    let stream_body = http_body_util::StreamBody::new(
                                        futures::stream::iter(chunks)
                                    );
                                    Ok::<_, hyper::Error>(hyper::Response::new(stream_body))
                                }
                            });
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, svc).await;
                        });
                    }
                });

                for _ in 0..3 {
                    let resp = client.get(&format!("http://{}/test", addr)).send().await.unwrap();
                    let _ = resp.bytes().await.unwrap();
                }

                let mut timings = Vec::new();
                for _ in 0..iters {
                    let t = Instant::now();
                    let resp = client.get(&format!("http://{}/test", addr)).send().await.unwrap();
                    let _ = resp.bytes().await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("L5X", "stream_get", 0, size, iters, &mut timings);
                emit(&r); results.push(r);
            }

            // --- L5f: hyper streamed body + writev + bigbuf ---
            {
                let response_bytes = bytes::Bytes::from(data.clone());
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let rb = response_bytes.clone();
                tokio::spawn(async move {
                    loop {
                        let (stream, _) = listener.accept().await.unwrap();
                        stream.set_nodelay(true).ok();
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let body = rb.clone();
                        tokio::spawn(async move {
                            let svc = hyper::service::service_fn(move |_req: hyper::Request<hyper::body::Incoming>| {
                                let body = body.clone();
                                async move {
                                    let len = body.len();
                                    let chunk_size = 4 * 1024 * 1024;
                                    let chunks: Vec<Result<hyper::body::Frame<bytes::Bytes>, std::convert::Infallible>> =
                                        (0..len).step_by(chunk_size).map(|start| {
                                            let end = (start + chunk_size).min(len);
                                            Ok(hyper::body::Frame::data(body.slice(start..end)))
                                        }).collect();
                                    let stream_body = http_body_util::StreamBody::new(
                                        futures::stream::iter(chunks)
                                    );
                                    Ok::<_, hyper::Error>(hyper::Response::new(stream_body))
                                }
                            });
                            let _ = hyper::server::conn::http1::Builder::new()
                                .writev(true)
                                .max_buf_size(4 * 1024 * 1024)
                                .serve_connection(io, svc).await;
                        });
                    }
                });

                for _ in 0..3 {
                    let resp = client.get(&format!("http://{}/test", addr)).send().await.unwrap();
                    let _ = resp.bytes().await.unwrap();
                }

                let mut timings = Vec::new();
                for _ in 0..iters {
                    let t = Instant::now();
                    let resp = client.get(&format!("http://{}/test", addr)).send().await.unwrap();
                    let _ = resp.bytes().await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("L5X", "all_opts_get", 0, size, iters, &mut timings);
                emit(&r); results.push(r);
            }
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
                stream.set_nodelay(true).ok();
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

    // ---- EXP: experimental pipeline variants ----
    // Tests different encode/decode strategies to find the fastest approach.
    // Compared against L4 put_stream and L6 GET baselines.
    if layers.contains(&"EXP".to_string()) {
        eprintln!("--- EXP: pipeline experiments ---");

        // --- EXP-GET: chunked mmap yield vs single yield ---
        // Hypothesis: yielding 4MB slices lets hyper start TCP writes earlier
        {
            let (_base, paths) = setup(1);
            let pool = Arc::new(make_pool(&paths));
            pool.make_bucket("bench").await.unwrap();

            for &size in &sizes {
                if size < 1024 * 1024 { continue; } // skip small sizes
                let data = vec![0x42u8; size];
                let label = human(size);
                let iters = iters_for_size(size, base_iters);

                // write test objects
                for i in 0..(iters + 2) {
                    pool.put_object("bench", &format!("exp_get/{}/{}", label, i), &data, opts()).await.unwrap();
                }

                // baseline: current single-Bytes yield (same as L4 get_stream)
                let mut timings = Vec::new();
                for i in 0..iters {
                    use futures::StreamExt;
                    let t = Instant::now();
                    let (_info, stream) = pool.get_object_stream("bench", &format!("exp_get/{}/{}", label, i)).await.unwrap();
                    let mut stream = std::pin::pin!(stream);
                    while let Some(chunk) = stream.next().await {
                        let _ = chunk.unwrap();
                    }
                    timings.push(t.elapsed());
                }
                let r = measure("EXP", "get_single", 1, size, iters, &mut timings);
                emit(&r); results.push(r);

                // experiment: re-chunk into 4MB slices (simulates what chunked mmap would do)
                let mut timings = Vec::new();
                for i in 0..iters {
                    use futures::StreamExt;
                    let t = Instant::now();
                    let (_info, stream) = pool.get_object_stream("bench", &format!("exp_get/{}/{}", label, i)).await.unwrap();
                    let mut stream = std::pin::pin!(stream);
                    // collect then re-chunk (simulates chunked mmap yield)
                    let mut all = bytes::BytesMut::new();
                    while let Some(chunk) = stream.next().await {
                        all.extend_from_slice(&chunk.unwrap());
                    }
                    let frozen = all.freeze();
                    let chunk_size = 4 * 1024 * 1024;
                    let mut offset = 0;
                    while offset < frozen.len() {
                        let end = (offset + chunk_size).min(frozen.len());
                        let _slice = frozen.slice(offset..end);
                        offset = end;
                    }
                    timings.push(t.elapsed());
                }
                let r = measure("EXP", "get_4mb_chunk", 1, size, iters, &mut timings);
                emit(&r); results.push(r);
            }
        }

        // --- EXP-PUT: pipeline variants ---
        // All variants write 1GB through the same storage layer.
        // Stream is pre-created (instant yield) so we measure encode+write
        // pipeline overhead, not network overlap benefit.
        {
            let (_base, paths) = setup(1);
            let pool = make_pool(&paths);
            pool.make_bucket("bench").await.unwrap();

            for &size in &sizes {
                if size < 1024 * 1024 { continue; }
                let data = vec![0x42u8; size];
                let label = human(size);
                let iters = iters_for_size(size, base_iters);

                // baseline: current put_stream (1MB blocks, sequential)
                let mut timings = Vec::new();
                for i in 0..iters {
                    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = data
                        .chunks(64 * 1024)
                        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
                        .collect();
                    let stream = futures::stream::iter(chunks);
                    let t = Instant::now();
                    pool.put_object_stream("bench", &format!("exp_1mb/{}/{}", label, i), stream, opts(), None, None).await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("EXP", "put_1mb_seq", 1, size, iters, &mut timings);
                emit(&r); results.push(r);

                // variant C: 4MB input chunks (tests if larger chunks help)
                let mut timings = Vec::new();
                for i in 0..iters {
                    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = data
                        .chunks(4 * 1024 * 1024)
                        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
                        .collect();
                    let stream = futures::stream::iter(chunks);
                    let t = Instant::now();
                    pool.put_object_stream("bench", &format!("exp_4mb/{}/{}", label, i), stream, opts(), None, None).await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("EXP", "put_4mb_chunk", 1, size, iters, &mut timings);
                emit(&r); results.push(r);

                // variant B: channel pipeline (RustFS style)
                // Spawn task to read+encode, main task writes. Uses mpsc(8).
                let mut timings = Vec::new();
                for i in 0..iters {
                    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = data
                        .chunks(64 * 1024)
                        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
                        .collect();
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
                    let t = Instant::now();

                    // producer: read stream into channel
                    let producer = tokio::spawn(async move {
                        use futures::StreamExt;
                        let mut stream = futures::stream::iter(chunks);
                        let mut buf = Vec::with_capacity(1024 * 1024);
                        while let Some(chunk) = stream.next().await {
                            let chunk = chunk.unwrap();
                            buf.extend_from_slice(&chunk);
                            while buf.len() >= 1024 * 1024 {
                                let block: Vec<u8> = buf.drain(..1024 * 1024).collect();
                                if tx.send(bytes::Bytes::from(block)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        if !buf.is_empty() {
                            let _ = tx.send(bytes::Bytes::from(buf)).await;
                        }
                    });

                    // consumer: receive and write via put_object
                    let mut received = Vec::new();
                    while let Some(block) = rx.recv().await {
                        received.extend_from_slice(&block);
                    }
                    producer.await.unwrap();

                    // now write (simulates the write phase)
                    pool.put_object("bench", &format!("exp_chan/{}/{}", label, i), &received, opts()).await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("EXP", "put_chan_pipe", 1, size, iters, &mut timings);
                emit(&r); results.push(r);

                // variant A: double-buffer (MinIO style)
                // Pre-read next block while writing current
                let mut timings = Vec::new();
                for i in 0..iters {
                    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = data
                        .chunks(64 * 1024)
                        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
                        .collect();
                    let t = Instant::now();

                    // simulate double-buffer: accumulate into two alternating buffers
                    use futures::StreamExt;
                    let mut stream = futures::stream::iter(chunks);
                    let block_size = 1024 * 1024;
                    let mut buf_a = Vec::with_capacity(block_size);
                    let mut buf_b = Vec::with_capacity(block_size);
                    let mut blocks: Vec<Vec<u8>> = Vec::new();

                    // fill first buffer
                    while buf_a.len() < block_size {
                        match stream.next().await {
                            Some(Ok(chunk)) => buf_a.extend_from_slice(&chunk),
                            _ => break,
                        }
                    }

                    loop {
                        // start filling buf_b while we "process" buf_a
                        let _fill = {
                            // we can't actually overlap with a sync stream, but
                            // measure the buffer management overhead
                            while buf_b.len() < block_size {
                                match stream.next().await {
                                    Some(Ok(chunk)) => buf_b.extend_from_slice(&chunk),
                                    _ => break,
                                }
                            }
                        };

                        // "process" buf_a (store the block)
                        if !buf_a.is_empty() {
                            let block: Vec<u8> = buf_a.drain(..).collect();
                            blocks.push(block);
                        } else {
                            break;
                        }

                        // swap
                        std::mem::swap(&mut buf_a, &mut buf_b);
                    }
                    // flush remaining
                    if !buf_b.is_empty() {
                        blocks.push(buf_b);
                    }

                    let all: Vec<u8> = blocks.into_iter().flatten().collect();
                    pool.put_object("bench", &format!("exp_dbuf/{}/{}", label, i), &all, opts()).await.unwrap();
                    timings.push(t.elapsed());
                }
                let r = measure("EXP", "put_dbl_buf", 1, size, iters, &mut timings);
                emit(&r); results.push(r);
            }
        }

        eprintln!();
    }

    // ---- L6A: S3 protocol (s3s + storage, WITH auth, raw reqwest) ----
    // Isolates auth overhead: same as L6 but with SigV4 auth provider enabled.
    // reqwest sends unsigned PUT (no SigV4 on client), but the server has auth
    // configured. This tells us if the auth provider presence slows anything.
    if layers.contains(&"L6A".to_string()) {
        eprintln!("--- L6A: S3 protocol + auth (reqwest) ---");

        let (_base, paths) = setup(1);
        let pool = Arc::new(make_pool(&paths));
        pool.make_bucket("bench").await.unwrap();

        let cluster = Arc::new(
            ClusterManager::new(ClusterConfig {
                node_id: "test".to_string(),
                advertise_s3: "http://127.0.0.1:0".to_string(),
                advertise_cluster: "http://127.0.0.1:0".to_string(),
                nodes: Vec::new(),
                access_key: "test".to_string(),
                secret_key: "testsecret".to_string(),
                no_auth: false,
                disk_paths: paths.clone(),
            })
            .unwrap(),
        );

        let s3 = abixio::s3_service::AbixioS3::new(Arc::clone(&pool), Arc::clone(&cluster));
        let mut builder = s3s::service::S3ServiceBuilder::new(s3);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "testsecret"));
        builder.set_access(abixio::s3_access::AbixioAccess::new(Arc::clone(&cluster)));
        builder.set_validation(abixio::s3_service::RelaxedNameValidation);
        let s3_service = builder.build();
        let dispatch = Arc::new(AbixioDispatch::new(s3_service, None, None));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dispatch_clone = dispatch.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                stream.set_nodelay(true).ok();
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
                let url = format!("http://{}/bench/l6aw_{}", addr, i);
                let _ = client.put(&url).body(data.clone()).send().await;
            }

            // PUT (unsigned, no SigV4 on client -- server sees anonymous request)
            let mut timings = Vec::new();
            for i in 0..iters {
                let url = format!("http://{}/bench/l6a_{}_{}", addr, label, i);
                let t = Instant::now();
                let resp = client.put(&url).body(data.clone()).send().await.unwrap();
                assert!(resp.status().is_success() || resp.status().as_u16() == 403,
                    "L6A PUT failed: {} (auth may reject unsigned -- that's ok for perf measurement)", resp.status());
                timings.push(t.elapsed());
            }
            let r = measure("L6A", "s3s_put_auth", 1, size, iters, &mut timings);
            emit(&r); results.push(r);
        }
        eprintln!();
    }

    // ---- L7: Full client path (s3s + storage + auth + aws-sdk-s3) ----
    // This is what real clients see. aws-sdk-s3 with SigV4 + UNSIGNED-PAYLOAD.
    // Same as the matrix benchmark but in-process (eliminates process boundary).
    if layers.contains(&"L7".to_string()) {
        eprintln!("--- L7: Full client path (aws-sdk-s3 + auth) ---");

        let (_base, paths) = setup(1);
        let pool = Arc::new(make_pool(&paths));
        pool.make_bucket("bench").await.unwrap();

        let cluster = Arc::new(
            ClusterManager::new(ClusterConfig {
                node_id: "test".to_string(),
                advertise_s3: "http://127.0.0.1:0".to_string(),
                advertise_cluster: "http://127.0.0.1:0".to_string(),
                nodes: Vec::new(),
                access_key: "test".to_string(),
                secret_key: "testsecret".to_string(),
                no_auth: false,
                disk_paths: paths.clone(),
            })
            .unwrap(),
        );

        let s3 = abixio::s3_service::AbixioS3::new(Arc::clone(&pool), Arc::clone(&cluster));
        let mut builder = s3s::service::S3ServiceBuilder::new(s3);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "testsecret"));
        builder.set_access(abixio::s3_access::AbixioAccess::new(Arc::clone(&cluster)));
        builder.set_validation(abixio::s3_service::RelaxedNameValidation);
        let s3_service = builder.build();
        let dispatch = Arc::new(AbixioDispatch::new(s3_service, None, None));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dispatch_clone = dispatch.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                stream.set_nodelay(true).ok();
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

        // build aws-sdk-s3 client pointing at our in-process server
        let endpoint = format!("http://127.0.0.1:{}", addr.port());
        let creds = aws_credential_types::provider::SharedCredentialsProvider::new(
            aws_credential_types::Credentials::new("test", "testsecret", None, None, "bench"),
        );
        let sdk_config = aws_config::SdkConfig::builder()
            .region(aws_config::Region::new("us-east-1"))
            .endpoint_url(&endpoint)
            .credentials_provider(creds)
            .behavior_version(aws_config::BehaviorVersion::latest())
            .build();
        let s3_client = aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::config::Builder::from(&sdk_config)
                .force_path_style(true)
                .build(),
        );

        for &size in &sizes {
            let data = vec![0x42u8; size];
            let iters = iters_for_size(size, base_iters);
            let label = human(size);

            // warmup (unsigned payload, same as matrix bench)
            for i in 0..3 {
                let body = aws_smithy_types::byte_stream::ByteStream::from(data.clone());
                let _ = s3_client.put_object()
                    .bucket("bench")
                    .key(format!("l7w_{}", i))
                    .content_type("application/octet-stream")
                    .body(body)
                    .customize()
                    .disable_payload_signing()
                    .send()
                    .await;
            }

            // PUT (unsigned payload -- same as matrix bench)
            let mut timings = Vec::new();
            for i in 0..iters {
                let body = aws_smithy_types::byte_stream::ByteStream::from(data.clone());
                let t = Instant::now();
                let result = s3_client.put_object()
                    .bucket("bench")
                    .key(format!("l7_{}_{}", label, i))
                    .content_type("application/octet-stream")
                    .body(body)
                    .customize()
                    .disable_payload_signing()
                    .send()
                    .await;
                timings.push(t.elapsed());
                assert!(result.is_ok(), "L7 PUT failed: {:?}", result.err());
            }
            let r = measure("L7", "sdk_put", 1, size, iters, &mut timings);
            emit(&r); results.push(r);

            // GET
            let mut timings = Vec::new();
            for i in 0..iters {
                let t = Instant::now();
                let result = s3_client.get_object()
                    .bucket("bench")
                    .key(format!("l7_{}_{}", label, i))
                    .send()
                    .await;
                if let Ok(resp) = result {
                    let _ = resp.body.collect().await;
                }
                timings.push(t.elapsed());
            }
            let r = measure("L7", "sdk_get", 1, size, iters, &mut timings);
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
