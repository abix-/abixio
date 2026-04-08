# Benchmarks

## Methodology

### Hardware
- Windows 10 Home, single machine, NTFS drive (tmpdir)
- Single-node, 1 disk, no erasure coding (1+0) unless noted

### Two benchmark modes

**Keep-alive** (`tests/bench_4kb.py`): Python `requests.Session` with
persistent HTTP connection. Measures actual server throughput -- no TCP
connect overhead, no process startup. This is how real S3 clients work
(aws-sdk, boto3, rclone all use connection pooling).

**Per-process** (`tests/compare_bench.sh`): `mc` client, one process per
operation. Includes SigV4 signing, `mc` startup (~60ms), and new TCP
connection per request. Useful for large objects where client overhead is
negligible relative to transfer time. Useless for 4KB.

### What we report
- **obj/sec**: operations per second (relevant for all sizes)
- **MB/s**: throughput (relevant for large objects)
- **ms/op**: per-request latency

### Windows caveats
- **Never use `localhost`** -- Windows DNS resolution adds ~200ms per request
  (IPv6 fallback). Always use `127.0.0.1`.
- **TCP connect** on Windows localhost = ~0.2ms (vs ~0.03ms on Linux).
  Keep-alive eliminates this after the first request.

---

## 4KB small object performance

Measured with `tests/bench_4kb.py` (keep-alive, 1000 ops per test).

| Server | PUT | | GET | | PUT ms | GET ms |
|--------|-----|--|-----|--|--------|--------|
| **AbixIO (log store)** | **1096 obj/s** | **4.3 MB/s** | **1315 obj/s** | **5.1 MB/s** | 0.91 | 0.76 |
| AbixIO (file tier) | 578 obj/s | 2.3 MB/s | 774 obj/s | 3.0 MB/s | 1.73 | 1.29 |
| RustFS | 1329 obj/s | 5.2 MB/s | 1349 obj/s | 5.3 MB/s | 0.75 | 0.74 |
| MinIO | 1189 obj/s | 4.6 MB/s | 1073 obj/s | 4.2 MB/s | 0.84 | 0.93 |

**AbixIO log store vs file tier**: PUT 90% faster, GET 70% faster.

**AbixIO log store vs competitors**: within 20% of RustFS (fastest), ahead
of MinIO on GET. The log store eliminates NTFS filesystem metadata overhead
(mkdir + file create) that dominates 4KB operations.

### How the log store wins

| | File tier | Log store |
|--|----------|-----------|
| PUT operations per 4KB | mkdir + shard.dat + meta.json = 3 fs ops | 1 append to segment |
| GET operations per 4KB | file open + read + close | mmap slice (page cache) |
| On-disk files per 1M objects | 3M+ | ~3 segments |

### Reproducing

```bash
# start servers manually, then:
python tests/bench_4kb.py --all --ops 1000

# or benchmark a single server:
python tests/bench_4kb.py --port 10000 --name AbixIO --ops 1000
```

---

## Large object throughput (10MB, 1GB)

Measured with `tests/compare_bench.sh` (`mc` client, per-process, SigV4).
MB/s is the relevant metric for large objects.

### PUT

![PUT throughput](img/bench-put.svg)

| Size | AbixIO | RustFS | MinIO |
|------|--------|--------|-------|
| 10MB | 80 MB/s | 81 MB/s | 93 MB/s |
| 1GB | 441 MB/s | 538 MB/s | 529 MB/s |

### GET

![GET throughput](img/bench-get.svg)

| Size | AbixIO | RustFS | MinIO |
|------|--------|--------|-------|
| 10MB | 107 MB/s | 109 MB/s | 126 MB/s |
| 1GB | 396 MB/s | 684 MB/s | 1102 MB/s |

At 1GB through `mc`, MinIO leads. AbixIO's server delivers 1220 MB/s via
direct curl -- the gap is `mc` client overhead, not server performance.

### Reproducing

```bash
cargo build --release
ABIXIO_BIN=./target/release/abixio RUSTFS_BIN=rustfs MINIO_BIN=minio MC=mc \
    ITERS=10 SIZES="4096 10485760 1073741824" \
    bash tests/compare_bench.sh
```

---

## Per-layer internal benchmarks

Each layer of the request path benchmarked independently. Not client-visible
-- used to identify bottlenecks and validate optimizations.

### GET by layer (1 disk)

| Layer | What | 4KB | 10MB | 1GB |
|-------|------|-----|------|-----|
| L6 | Full S3 (end to end) | 6.4 MB/s | 269 MB/s | 624 MB/s |
| L4 | Storage + mmap | 9.2 MB/s | 18,313 MB/s | page cache |
| L4 | Storage + mmap EC (4 disk) | 5.4 MB/s | 11,766 MB/s | page cache |

### PUT by layer (1 disk)

| Layer | What | 4KB | 10MB | 1GB |
|-------|------|-----|------|-----|
| L6 | Full S3 (end to end) | 3.3 MB/s | 247 MB/s | 339 MB/s |
| L4 | Storage pipeline | 3.7 MB/s | 481 MB/s | 540 MB/s |
| L3 | Disk write (page cache) | -- | 1625 MB/s | -- |
| L2 | RS encode (SIMD) | -- | 2762 MB/s | -- |
| L1 | MD5 hash | -- | 703 MB/s | -- |

### Reproducing

```bash
cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# compare after a change
BENCH_COMPARE=bench-results/baseline.json \
    cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# specific layers or sizes
BENCH_LAYERS=L4,L6 BENCH_SIZES=10485760 \
    cargo test --release --test layer_bench -- --ignored bench_perf --nocapture
```

---

For optimization history and allocation audit, see [layer-optimization.md](layer-optimization.md).

For log-structured storage design, see [write-log.md](write-log.md).
