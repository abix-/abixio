# Benchmarks

## Methodology

### Fairness principles

Every client must run the same test under the same conditions. If one
client gets an advantage (keep-alive, memory I/O, more warmup), the
comparison is not valid. These rules ensure parity:

1. **Same warmup**: 3 PUT + 3 GET warmup operations before timing, for
   every client. This ensures TCP connections are established, caches are
   warm, and JIT (if any) has run.

2. **Same I/O model**: all clients read PUT payload from a temp file on
   disk. All clients write GET output to a temp file on disk. No client
   gets the advantage of in-memory I/O while others do disk I/O.

3. **Same connection model**: each client uses a single connection with
   keep-alive across all iterations. For per-process CLI tools (mc,
   rclone), process spawn overhead is measured and subtracted so we
   measure transfer time, not startup time.

4. **Same iterations**: all clients run the same number of iterations
   per (server, size) combination.

5. **Same payload**: identical byte pattern, identical size, identical
   content-type for all clients.

6. **Same auth**: all clients use SigV4 with the same credentials.
   UNSIGNED-PAYLOAD for PUT body (standard for HTTPS clients).

7. **Same server config**: each server runs single-node, 1 disk, NTFS
   tmpdir, same machine, release build. Servers are started fresh for
   each benchmark run.

### Test infrastructure

All benchmarks run through `abixio-ui/tests/bench.rs` -- a Rust test harness
that starts real server processes, creates temp dirs, and benchmarks through
real S3 clients. No synthetic tests, no mocked storage. Real servers, real
clients, real data.

### Clients

| Client | Type | Auth | Connection | Overhead |
|--------|------|------|-----------|----------|
| **aws-sdk-s3** (Rust) | In-process SDK | SigV4, UNSIGNED-PAYLOAD | Keep-alive | None |
| **mc** (MinIO client) | Per-process CLI | SigV4 | New process per op | ~41ms (subtracted) |
| **rclone** | Per-process CLI | SigV4 | New process per op | ~111ms (subtracted) |

For CLI tools, process spawn overhead is measured before each benchmark
run (`mc ls` / `rclone lsd`) and subtracted from every timing. This gives
transfer-only throughput. The overhead is printed in the output for
transparency.

### Servers

All servers run single-node, 1 disk, NTFS tmpdir, same machine (Windows 10).

- **AbixIO** -- Rust S3 server with RAM write cache, log-structured storage, mmap GET
- **RustFS** 1.0.0-alpha.90 -- Rust S3 server (MinIO-compatible)
- **MinIO** RELEASE.2026-04-07 -- Go S3 server (reference implementation)

All binaries must be release builds. Debug builds are 5-7x slower.

### What we report

- **obj/sec** -- operations per second (primary metric for small objects)
- **MB/s** -- throughput (primary metric for large objects)
- **latency** -- per-request time in microseconds or milliseconds

### Known limitations

- **Windows-only**: all benchmarks run on Windows 10. Linux numbers may
  differ due to epoll vs IOCP, different TCP stack behavior, and different
  filesystem performance.
- **Single machine**: client and server share CPU, memory, and network
  stack. No network latency. Results represent localhost throughput, not
  production deployment.
- **Per-process CLI overhead subtraction**: uses a lightweight operation
  (`ls`/`lsd`) to estimate overhead. Actual `cp` process overhead may be
  slightly higher due to additional argument parsing and file open. Small
  objects may show inflated throughput after subtraction.
- **Iteration count**: 3 iterations for 1GB may show variance. L7 layer
  bench with 10 iterations is more reliable for large-object numbers.

### Windows caveats

- Always use `127.0.0.1`, never `localhost` (Windows DNS adds ~200ms)
- TCP connect on Windows loopback = ~0.2ms (Linux = ~0.03ms)
- TCP_NODELAY must be set explicitly (Go sets it by default)
- hyper needs `writev(true)` + `max_buf_size(4MB)` for optimal throughput

---

## Comprehensive matrix

3 servers x 3 clients x 3 sizes x 2 operations.
Run with: `cd abixio-ui && cargo test --release --test bench -- --ignored --nocapture bench_matrix`

**Status**: the current benchmark code does NOT yet meet all fairness
principles above. Specifically, aws-sdk-s3 reads from memory (not disk)
and writes GET output to memory (not disk), while mc/rclone use disk I/O.
The code needs to be updated to enforce parity. Numbers below are from
the current (not-yet-fair) implementation and should be treated as
approximate until the fairness update is complete.

### 4KB -- small object performance (obj/sec)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET |
|--------|---------------|---------------|--------|--------|
| **AbixIO** | **1923** | **2526** | 23 | 24 |
| MinIO | 432 | 1531 | 22 | 22 |
| RustFS | 353 | 874 | 21 | 23 |

**AbixIO has the fastest 4KB PUT and GET** thanks to RAM write cache and
log-structured storage. 4.5x faster PUT than MinIO, 1.6x faster GET.
mc shows ~22 obj/s for all servers -- process spawn overhead dominates.

### 10MB -- medium object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET |
|--------|---------------|---------------|--------|--------|
| **AbixIO** | **325** | 399 | **111** | 156 |
| RustFS | **308** | 337 | 96 | 149 |
| MinIO | 171 | **648** | **116** | **186** |

AbixIO leads PUT (325 MB/s vs RustFS 308, MinIO 171). MinIO leads 10MB
GET (648 MB/s) -- Go's net/http has lower per-response overhead. L5X
micro-benchmarks show hyper can reach 726 MB/s with tuned config; the
remaining gap is s3s/ChunkedBytes overhead between L5X and the full stack.

### 1GB -- large object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET |
|--------|---------------|---------------|--------|--------|
| **AbixIO** | **546** | 674 | 435 | 465 |
| RustFS | 387 | 663 | **557** | 573 |
| MinIO | 432 | **796** | **670** | **681** |

**AbixIO has the fastest 1GB PUT** through aws-sdk-s3 (546 MB/s vs MinIO
432, RustFS 387) thanks to xxhash64 ETag (skips MD5 when client doesn't
send Content-MD5). 1GB GET within 15% of MinIO (674 vs 796 MB/s) with
writev(true), max_buf_size(4MB), TCP_NODELAY, and zero-copy chunked mmap.

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
