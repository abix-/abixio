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
| **AbixIO** | **1777** | **2530** | 24 | 24 |
| MinIO | 436 | 1528 | 21 | 24 |
| RustFS | 375 | 931 | 22 | 23 |

**AbixIO has the fastest 4KB PUT and GET** thanks to RAM write cache and
log-structured storage. 4.1x faster PUT than MinIO, 1.7x faster GET.
mc shows ~23 obj/s for all servers -- process spawn overhead dominates.

### 10MB -- medium object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET |
|--------|---------------|---------------|--------|--------|
| **AbixIO** | **247** | **440** | **111** | **157** |
| RustFS | **312** | 323 | 98 | 147 |
| MinIO | 231 | **685** | 117 | 174 |

AbixIO leads 10MB GET through aws-sdk-s3 over RustFS (440 vs 323 MB/s)
thanks to ChunkedBytes response path. MinIO leads 10MB GET (685 MB/s) --
hyper's HTTP write ceiling on Windows is 510 MB/s for raw 10MB bodies,
which is the fundamental limit vs Go's net/http.

### 1GB -- large object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET |
|--------|---------------|---------------|--------|--------|
| **AbixIO** | **578** | 584 | 437 | 424 |
| RustFS | 381 | **648** | **567** | **659** |
| MinIO | 436 | **811** | 557 | **727** |

**AbixIO has the fastest 1GB PUT** through aws-sdk-s3 (578 MB/s vs MinIO
436, RustFS 381) thanks to xxhash64 ETag (skips MD5 when client doesn't
send Content-MD5) and TCP_NODELAY. 1GB GET trails MinIO by 28% (584 vs
811 MB/s) -- the remaining gap is hyper's TCP write efficiency vs Go's
net/http on Windows (confirmed by L5 raw hyper ceiling measurement).

---

## Single-server detailed benchmark

AbixIO 1-disk, all operations, all sizes.
Run with: `cd abixio-ui && cargo test --release --test bench -- --ignored --nocapture bench_1_disk`

```
OP       SIZE          ops         avg         p50         p99          MB/s     obj/sec
PUT      4KB       500 ops     513.4us     502.1us     679.6us      7.6 MB/s        1948
GET      4KB       500 ops     345.3us     331.5us     485.0us     11.3 MB/s        2896
HEAD     4KB       500 ops     325.8us     318.3us     408.6us             -        3070
DELETE   4KB       500 ops     432.0us     431.6us     587.3us             -        2315
PUT      1KB       100 ops     469.3us     462.5us     575.4us      2.1 MB/s        2131
PUT      1MB        20 ops      12.5ms      11.5ms      19.4ms     79.7 MB/s          80
PUT      10MB        5 ops      89.6ms      88.8ms      90.4ms    111.7 MB/s          11
PUT*     10MB        5 ops      38.9ms      40.5ms      45.2ms    257.0 MB/s          26
GET      1KB       100 ops     396.6us     388.5us     588.5us      2.5 MB/s        2522
GET      1MB        20 ops       2.6ms       2.6ms       2.8ms    382.1 MB/s         382
GET      10MB        5 ops      16.5ms      15.1ms      16.1ms    605.4 MB/s          61
HEAD     -         100 ops     376.8us     372.0us     473.9us             -        2654
LIST     100obj     50 ops       4.7ms       4.7ms       5.0ms             -         214
DELETE   1KB       100 ops     430.2us     417.0us     600.8us             -        2325
```

`PUT*` = UNSIGNED-PAYLOAD (skips client-side SHA256). Note the 2.6x
speedup vs signed PUT at 10MB (257 vs 112 MB/s).

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
