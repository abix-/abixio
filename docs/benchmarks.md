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
| **AbixIO** | **442** | 578 | 23 | 24 |
| MinIO | 420 | **1463** | 23 | 25 |
| RustFS | 361 | 959 | 23 | 24 |

**AbixIO has the fastest 4KB PUT** thanks to the RAM write cache and
log-structured storage. MinIO leads GET (2.5x faster than AbixIO).
mc shows ~23 obj/s for all servers -- process spawn overhead dominates.

### 10MB -- medium object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET |
|--------|---------------|---------------|--------|--------|
| AbixIO | 50 | 241 | 15 | 153 |
| **RustFS** | **319** | 301 | 100 | 138 |
| MinIO | 165 | **775** | **116** | **204** |

AbixIO 10MB PUT is slow through aws-sdk-s3 (50 MB/s). The streaming encode
path through s3s has overhead that RustFS and MinIO don't pay. This is the
primary optimization target for large objects.

### 1GB -- large object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET |
|--------|---------------|---------------|--------|--------|
| AbixIO | 52 | 577 | 60 | 431 |
| RustFS | 383 | 682 | 557 | 736 |
| **MinIO** | **415** | **817** | **689** | **868** |

AbixIO 1GB PUT is 7x slower than MinIO (52 vs 415 MB/s). GET is
respectable (577 MB/s, 71% of MinIO) thanks to mmap zero-copy.
mc is faster than aws-sdk-s3 for large PUT on RustFS/MinIO because
mc handles connection and transfer more efficiently for bulk data.

---

## Single-server detailed benchmark

AbixIO 1-disk, all operations, all sizes.
Run with: `cd abixio-ui && cargo test --release --test bench -- --ignored --nocapture bench_1_disk`

```
OP       SIZE          ops         avg         p50         p99          MB/s     obj/sec
PUT      4KB       500 ops     565.0us     534.9us     897.8us      6.9 MB/s        1770
GET      4KB       500 ops     344.8us     333.0us     492.1us     11.3 MB/s        2900
HEAD     4KB       500 ops     356.5us     355.3us     466.3us             -        2805
DELETE   4KB       500 ops     489.0us     445.0us     737.8us             -        2045
PUT      1KB       100 ops     503.6us     489.2us     661.5us      1.9 MB/s        1986
PUT      1MB        20 ops      13.1ms      12.9ms      13.9ms     76.0 MB/s          76
PUT      10MB        5 ops     104.0ms     102.4ms     104.9ms     96.1 MB/s          10
PUT*     10MB        5 ops      39.7ms      32.7ms      47.2ms    251.8 MB/s          25
GET      1KB       100 ops     411.2us     410.9us     509.1us      2.4 MB/s        2432
GET      1MB        20 ops       2.7ms       2.7ms       3.0ms    365.8 MB/s         366
GET      10MB        5 ops      42.2ms      40.4ms      45.9ms    236.8 MB/s          24
HEAD     -         100 ops     373.2us     349.3us     475.3us             -        2679
LIST     100obj     50 ops       4.9ms       4.9ms       5.1ms             -         205
DELETE   1KB       100 ops     467.4us     443.1us     695.1us             -        2140
```

`PUT*` = UNSIGNED-PAYLOAD (skips client-side SHA256). Note the 2.5x
speedup vs signed PUT at 10MB (252 vs 96 MB/s).

---

## Client comparison

Same AbixIO server, different S3 clients.
Run with: `cd abixio-ui && cargo test --release --test bench -- --ignored --nocapture bench_clients`

| Client | 4KB PUT | 4KB GET | Latency |
|--------|---------|---------|---------|
| **aws-sdk-s3 (Rust)** | **414 obj/s** | **624 obj/s** | 2.4ms / 1.6ms |
| curl (unsigned) | 55 obj/s | 67 obj/s | 18ms / 15ms |
| mc (per-process) | 23 obj/s | 24 obj/s | 43ms / 41ms |

aws-sdk-s3 is 18x faster than mc for 4KB. mc process spawn (~40ms)
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
