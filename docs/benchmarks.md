# Benchmarks

## Methodology

### Test infrastructure

All benchmarks run through `abixio-ui/tests/bench.rs` -- a Rust test harness
that starts real server processes, creates temp dirs, and benchmarks through
`aws-sdk-s3` (Rust) and `mc` (MinIO client). No synthetic tests, no mocked
storage. Real servers, real S3 clients, real data.

### Clients

| Client | Type | Auth | Connection | When to use |
|--------|------|------|-----------|-------------|
| **aws-sdk-s3** (Rust) | In-process | SigV4, UNSIGNED-PAYLOAD | Keep-alive | Primary benchmark. Real SDK, real auth, connection reuse. |
| **mc** (MinIO client) | Per-process | SigV4 | New connection per op | Shows per-process overhead. Each `mc cp` = process spawn + TCP connect + transfer. |

aws-sdk-s3 uses UNSIGNED-PAYLOAD for PUT (same as mc, rclone, AWS CLI over
HTTPS). This skips client-side SHA256 of the body. All S3 benchmarks in
the wild use unsigned payloads.

### Servers

All servers run single-node, 1 disk, NTFS tmpdir, same machine (Windows 10).

- **AbixIO** -- our server with RAM write cache, log-structured storage, mmap GET
- **RustFS** 1.0.0-alpha.90 -- Rust S3 server (MinIO-compatible)
- **MinIO** RELEASE.2026-04-07 -- Go S3 server (reference implementation)

All binaries must be release builds. Debug builds are 5-7x slower.

### What we report

- **obj/sec** -- operations per second (primary metric for small objects)
- **MB/s** -- throughput (primary metric for large objects)
- **latency** -- per-request time in microseconds or milliseconds

### Windows caveats

- Always use `127.0.0.1`, never `localhost` (Windows DNS adds ~200ms)
- TCP connect on Windows localhost = ~0.2ms (Linux = ~0.03ms)
- aws-sdk-s3 keep-alive eliminates TCP connect after first request
- mc spawns a new process per operation (~40ms overhead)

---

## Comprehensive matrix

3 servers x 2 clients x 3 sizes x 2 operations.
Run with: `cd abixio-ui && cargo test --release --test bench -- --ignored --nocapture bench_matrix`

### 4KB -- small object performance (obj/sec)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET |
|--------|---------------|---------------|--------|--------|
| **AbixIO** | **1964** | **2872** | 25 | 26 |
| MinIO | 421 | 1442 | 24 | 25 |
| RustFS | 319 | 934 | 23 | 25 |

**AbixIO has the fastest 4KB PUT and GET** thanks to RAM write cache and
log-structured storage. 4.7x faster PUT and 2x faster GET than MinIO.
mc shows ~24 obj/s for all servers -- process spawn overhead dominates.

### 10MB -- medium object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET |
|--------|---------------|---------------|--------|--------|
| **AbixIO** | 246 | 339 | 101 | 155 |
| RustFS | **313** | 252 | 101 | 133 |
| MinIO | 178 | **755** | **118** | **203** |

AbixIO 10MB PUT is competitive (246 MB/s vs RustFS 313, MinIO 178).
MinIO leads GET (755 MB/s, 2.2x faster than AbixIO).

### 1GB -- large object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET |
|--------|---------------|---------------|--------|--------|
| AbixIO | 320 | 555 | 441 | 433 |
| RustFS | **386** | 669 | 602 | 673 |
| MinIO | 298 | **826** | **734** | **736** |

AbixIO 1GB PUT is competitive through aws-sdk-s3 (320 MB/s vs RustFS 386,
MinIO 298). GET is respectable (555 MB/s, 67% of MinIO) thanks to mmap
zero-copy. mc throughput trails RustFS/MinIO for large objects due to
mc's Go HTTP stack being optimized for Go servers.

---

## Single-server detailed benchmark

AbixIO 1-disk, all operations, all sizes.
Run with: `cd abixio-ui && cargo test --release --test bench -- --ignored --nocapture bench_1_disk`

```
OP       SIZE          ops         avg         p50         p99          MB/s     obj/sec
PUT      4KB       500 ops     533.4us     530.6us     663.9us      7.3 MB/s        1875
GET      4KB       500 ops     360.7us     361.6us     462.6us     10.8 MB/s        2772
HEAD     4KB       500 ops     352.6us     349.5us     441.9us             -        2836
DELETE   4KB       500 ops     487.8us     447.4us     799.7us             -        2050
PUT      1KB       100 ops     500.8us     493.1us     647.2us      2.0 MB/s        1997
PUT      1MB        20 ops      12.8ms      12.8ms      13.2ms     77.8 MB/s          78
PUT      10MB        5 ops     102.8ms     101.4ms     103.0ms     97.3 MB/s          10
PUT*     10MB        5 ops      43.5ms      41.7ms      47.4ms    230.1 MB/s          23
GET      1KB       100 ops     419.7us     415.9us     610.0us      2.3 MB/s        2383
GET      1MB        20 ops       2.7ms       2.7ms       3.0ms    370.5 MB/s         371
GET      10MB        5 ops      43.2ms      46.2ms      46.8ms    231.3 MB/s          23
HEAD     -         100 ops     386.4us     371.0us     471.9us             -        2588
LIST     100obj     50 ops       5.0ms       5.0ms       5.1ms             -         200
DELETE   1KB       100 ops     453.4us     441.3us     698.3us             -        2205
```

`PUT*` = UNSIGNED-PAYLOAD (skips client-side SHA256). Note the 2.5x
speedup vs signed PUT at 10MB (252 vs 96 MB/s).

---

## Client comparison

Same AbixIO server, different S3 clients.
Run with: `cd abixio-ui && cargo test --release --test bench -- --ignored --nocapture bench_clients`

| Client | 4KB PUT | 4KB GET | Latency |
|--------|---------|---------|---------|
| **aws-sdk-s3 (Rust)** | **1850 obj/s** | **2510 obj/s** | 541us / 398us |
| curl (unsigned) | 57 obj/s | 69 obj/s | 17ms / 14ms |
| mc (per-process) | 25 obj/s | 25 obj/s | 40ms / 40ms |

aws-sdk-s3 is 74x faster than mc for 4KB. mc process spawn (~40ms)
dominates small-object latency. For large objects, mc's overhead is
amortized and it's competitive.

---

## Server-side profiling

Debug header `x-debug-s3s-ms` shows actual server processing time
(excludes client overhead, TCP, HTTP parsing):

```
4KB PUT server processing:  0.28ms
4KB GET server processing:  0.08ms
4KB HEAD server processing: 0.08ms
```

The server processes 4KB GET in 80 microseconds. The remaining latency
in benchmarks is client overhead (SigV4 signing, HTTP, connection management).

---

## Internal per-layer benchmarks

Tests each layer of the storage stack independently (no HTTP client).
Run with: `cd abixio && cargo test --release --test layer_bench -- --ignored bench_perf --nocapture`

These measure the storage engine directly and are used for optimization
work. See [layer-optimization.md](layer-optimization.md) for details.

---

## Running benchmarks

```bash
# build AbixIO release binary first
cd /path/to/abixio && cargo build --release

# comprehensive matrix (3 servers, 2 clients, 3 sizes)
cd /path/to/abixio-ui
ABIXIO_BIN=/path/to/abixio/target/release/abixio \
    cargo test --release --test bench -- --ignored --nocapture bench_matrix

# single server detailed
cargo test --release --test bench -- --ignored --nocapture bench_1_disk

# client comparison
cargo test --release --test bench -- --ignored --nocapture bench_clients

# internal per-layer (runs in abixio repo, no external binaries)
cd /path/to/abixio
cargo test --release --test layer_bench -- --ignored bench_perf --nocapture
```

RustFS and MinIO binaries auto-detected at `C:\tools\rustfs.exe` and
`C:\tools\minio.exe`. Override with `RUSTFS_BIN` and `MINIO_BIN` env vars.

---

For optimization history and allocation audit, see [layer-optimization.md](layer-optimization.md).
For log-structured storage design, see [write-log.md](write-log.md).
For RAM write cache design, see [write-cache.md](write-cache.md).
