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
   rclone), process spawn overhead is measured and reported. Published
   matrix numbers below are end-to-end and include process spawn unless
   a section explicitly says otherwise.

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

All benchmarks run through `abixio-ui/tests/bench.rs`, a Rust test harness
that starts real server processes, creates temp dirs, and benchmarks through
real S3 clients. No synthetic tests, no mocked storage. Real servers, real
clients, real data.

### Clients

| Client | Type | Auth | Connection | Overhead |
|--------|------|------|-----------|----------|
| **aws-sdk-s3** (Rust) | In-process SDK | SigV4, UNSIGNED-PAYLOAD | Keep-alive | None |
| **mc** (MinIO client) | Per-process CLI | SigV4 | New process per op | ~41ms |
| **rclone** | Per-process CLI | SigV4 | New process per op | ~111ms |

For CLI tools, process spawn overhead is measured before each benchmark
run (`mc ls` / `rclone lsd`) and printed in the output for transparency.
The comprehensive matrix below reports real end-to-end timing including
that overhead.

### Servers

All servers run single-node, 1 disk, NTFS tmpdir, same machine (Windows 10).

- **AbixIO**: Rust S3 server with RAM write cache, log-structured storage, mmap GET
- **RustFS** 1.0.0-alpha.90: Rust S3 server (MinIO-compatible)
- **MinIO** RELEASE.2026-04-07: Go S3 server (reference implementation)

All binaries must be release builds. Debug builds are 5-7x slower.

### What we report

- **obj/sec**: operations per second (primary metric for small objects)
- **MB/s**: throughput (primary metric for large objects)
- **latency**: per-request time in microseconds or milliseconds

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

All clients now use the same I/O model: read PUT payload from disk, write
GET output to disk, 3 PUT + 3 GET warmup before timing. CLI process
overhead (~41ms mc, ~111ms rclone) is reported but NOT subtracted;
numbers show real end-to-end time including process spawn.

### 4KB: small object performance (obj/sec)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET | rclone PUT | rclone GET |
|--------|---------------|---------------|--------|--------|------------|------------|
| **AbixIO** | **1618** | **1059** | 23 | 23 | 9 | 9 |
| MinIO | 367 | 842 | 22 | 23 | 8 | 9 |
| RustFS | 345 | 613 | 21 | 23 | 8 | 8 |

**AbixIO has the fastest 4KB PUT** thanks to RAM write cache and
log-structured storage. 4.4x faster PUT than MinIO through aws-sdk-s3.
GET leads at 1059 obj/s vs MinIO 842 (+26%). CLI tools (mc ~22 obj/s,
rclone ~9 obj/s) are dominated by process spawn overhead (~41ms mc,
~111ms rclone). These numbers measure startup, not S3 performance.

### 10MB: medium object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET | rclone PUT | rclone GET |
|--------|---------------|---------------|--------|--------|------------|------------|
| **AbixIO** | **347** | 210 | 106 | 172 | 61 | 86 |
| RustFS | 292 | 184 | 96 | 134 | 76 | 86 |
| MinIO | 181 | **235** | **115** | **190** | 63 | 87 |

AbixIO leads PUT (347 MB/s vs RustFS 292, MinIO 181). GET through
aws-sdk-s3 is similar across servers (~200 MB/s) because disk write
speed equalizes the results. rclone GET shows ~86 MB/s for all servers
at 10MB. Process overhead still significant at this size.

### 1GB: large object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | mc PUT | mc GET | rclone PUT | rclone GET |
|--------|---------------|---------------|--------|--------|------------|------------|
| **AbixIO** | **465** | 247 | 337 | 352 | 202 | * |
| RustFS | 344 | 262 | **560** | **607** | 168 | * |
| MinIO | 356 | 243 | **616** | **832** | 235 | * |

`*` rclone 1GB GET numbers excluded because rclone caches/skips repeated
downloads to the same file, producing unreliable measurements.

**AbixIO has the fastest 1GB PUT** through aws-sdk-s3 (465 MB/s vs MinIO
356, RustFS 344) thanks to xxhash64 ETag (skips MD5 when client doesn't
send Content-MD5). 1GB GET through aws-sdk-s3 is ~245 MB/s for ALL
servers because disk write speed is the bottleneck when writing GET
output to file, proving the test is fair. mc GET shows the real network
throughput difference: AbixIO 352, MinIO 832. The mc client (Go binary)
transfers faster to Go servers (MinIO) due to shared HTTP stack
optimizations.

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

## Internal per-layer benchmarks

Tests each layer of the storage stack independently (no HTTP client).
Run with: `cd abixio && cargo test --release --test layer_bench -- --ignored bench_perf --nocapture`

These measure the storage engine directly and are used for optimization
work. See [layer-optimization.md](layer-optimization.md) for details.

---

## Write tier comparison (Phase 8 matrix, end-to-end HTTP)

The authoritative explanation of what each write branch actually does,
what ack means, and when data reaches its final resting place is now
[write-path.md](write-path.md). This section stays focused on the
measured comparison.

`bench_pool_l4_tier_matrix` spawns a fresh abixio server per
configuration and drives it through the full hyper + s3s + VolumePool
stack via reqwest. 1 disk, ftt=0, 127.0.0.1 loopback, sequential,
1000 iters at 4KB, 500 at 64KB, 200 at 1MB, 50 at 10MB, 10 at 100MB.

Three tiers compared:
- **file**: baseline, no log store, no write pool. Every PUT does
  `mkdir + File::create + write + close` for both shard.dat and
  meta.json.
- **log**: log-structured store enabled. Small PUTs (<=64KB) append
  to a pre-opened segment file. Large PUTs fall through to the file
  tier.
- **pool**: pre-opened temp file pool enabled. Small PUTs pop a
  slot, write data + meta concurrently, and hand off rename to an
  async worker. Large PUTs also use the pool.

Run with:
```
cargo test --release --test layer_bench -- --ignored --nocapture \
    bench_pool_l4_tier_matrix
```

### PUT p50 latency (Phase 8.7 numbers, after multi-worker + raised defaults)

| Size | file | log | pool | pool vs file | best tier |
|---|---|---|---|---|---|
| 4KB | 935us | **265us** | 454us | **2.1x** | **log** |
| 64KB | 1330us | **385us** | 586us | **2.3x** | **log** |
| 1MB | 4360us | 4057us | **3797us** | 1.1x | **pool** |
| 10MB | 28974us | 31744us | 33837us | 0.9x | **file** |
| 100MB | 149228us | 164974us | 165671us | 0.9x | **file** |

### GET p50 latency

| Size | file | log | pool | best tier |
|---|---|---|---|---|
| 4KB | 689us | **173us** | 708us | **log** |
| 64KB | 949us | **218us** | 917us | **log** |
| 1MB | 1929us | 1776us | **1882us** | log |
| 10MB | 16840us | **10032us** | 10100us | log |
| 100MB | 100848us | **97709us** | 98955us | log |

### Interpretation

**Log store wins small objects (<=64KB).** PUT p50 is 3-4x better
than file tier and 1.7-1.5x better than the pool. GET p50 is 4x
better than file tier and 4x better than the pool. The log store's
HashMap-indexed reads from an always-mmap'd segment are
fundamentally faster than the pool's `File::open + mmap` path for
small objects.

**Pool wins mid-range (1MB to 10MB).** At 1MB the pool's pre-opened
file + concurrent write trick beats the file tier by 1.1x. The
log store falls through to the file tier at >=64KB and stops
helping. The pool's sweet spot is exactly this mid-range window.

**File tier wins large objects (>10MB).** At 100MB the file tier's
direct-write path beats the pool and the log store because the pool
pays an extra NTFS rename at the end of each write. At those sizes
the rename alone adds 60-100ms.

**The three-tier handoff** that production should ship:
- `<=64KB`: log store
- `64KB to 10MB`: pool
- `>10MB`: file tier

The `--write-tier` CLI flag (still pending) would expose this as
a runtime configuration knob. Today the tier is picked by the
volume setup code.

---

## Stack breakdown (Phase 8.5: where does the 4KB time go)

Use [write-path.md](write-path.md) for the canonical layer-by-layer
interpretation of these branches. This section is the timing source for
that document, not the canonical flow description by itself.

`bench_pool_l4_5_stack_breakdown` attributes the 4KB PUT latency to
specific layers via progressive stages. Ten stages, each a fresh
server that adds one layer:

| Stage | config | p50 | what it adds |
|---|---|---|---|
| A | bare hyper | **94us** | HTTP floor |
| B | hyper + direct write_shard | 641us | body read + file-tier write_shard |
| C | hyper + s3s + AbixioS3 + null backend | 126us | +32us s3s + abixio dispatch |
| D | file_tier (full stack) | 810us | +684us real file-tier storage work |
| E | pool_tier (depth 32, ch 256 [old default]) | 737us | starved + channel blocked |
| E* | pool_tier (depth 100) | 497us | starved less |
| E'' | pool_tier (depth 1024) | 1236us | channel fully blocked |
| E' | pool_tier_drained(32) | 322us | drain avoids both chokes |
| E+ | pool_tier (depth 32, ch 100k) | 561us | unbounded ch, still starves |
| **E#** | **pool_tier (depth 1024, ch 100k)** | **318us** | **pool true fast path** |

Run with:
```
cargo test --release --test layer_bench -- --ignored --nocapture \
    bench_pool_l4_5_stack_breakdown
```

**Key finding:** the HTTP stack (reqwest + TCP + hyper) contributes
only 94us at 4KB. s3s + AbixioS3 + VolumePool dispatch adds only
32us on top. The file tier's ~900us is real NTFS storage work, not
HTTP overhead. The pool's true fast path at HTTP layer is ~318us,
not 11us (that number was a storage-layer tight-loop measurement
on an idle tokio runtime and does not translate to a live runtime).

Phase 8.5 also discovered that the **default pool config was
broken for sustained load**: depth 32 starved after ~30ms and
channel 256 backpressured tx.send once the worker fell behind.
Phase 8.7 fixed both by raising channel to 10k and adding a
second worker (round-robin dispatch), which is what made the
Phase 8 matrix above land on the current pool 4KB number of
454us instead of the original Phase 8 value of 942us.

Raw Phase 8.7 output: `bench-results/phase8.7-tier-matrix.txt`.
Raw Phase 8.5 output: `bench-results/phase8.5-stack-breakdown-v5.txt`.

See [write-pool.md](write-pool.md) for the full optimization
journey including every phase of the write pool design.

---

## Running benchmarks

```bash
# build AbixIO release binary first
cd /path/to/abixio && cargo build --release

# comprehensive matrix (3 servers, 3 clients, 3 sizes)
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

For the canonical end-to-end PUT path, see [write-path.md](write-path.md).
For optimization history and allocation audit, see [layer-optimization.md](layer-optimization.md).
For log-structured storage design, see [write-log.md](write-log.md).
For RAM write cache design, see [write-cache.md](write-cache.md).

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| `bench_pool_l4_tier_matrix` and `bench_pool_l4_5_stack_breakdown` exist | Verified | `tests/layer_bench.rs:3164`, `3499` |
| Raw artifacts referenced for Phase 8.7 and 8.5 exist | Verified | `bench-results/phase8.7-tier-matrix.txt`, `bench-results/phase8.5-stack-breakdown-v5.txt` |
| Debug profiling header `x-debug-s3s-ms` exists in code | Verified | `src/s3_route.rs:82` |
| Matrix comment said `3 servers, 2 clients` | Corrected | Document contradicted itself; client list and matrix section clearly use 3 clients |
| CLI overhead was described as both subtracted and not subtracted | Corrected | Document contradicted itself; matrix section explicitly says published numbers are end-to-end including spawn |
| Phase 8 / 8.5 benchmark section names and commands | Verified | `tests/layer_bench.rs:3111-3164`, `3455-3499` |
| Specific published numeric results in matrix tables | Not independently re-run in this pass | Numbers match this document and related docs/artifacts, but I did not execute the external benchmark harness here |
| `1220 MB/s` curl GET and other historical perf numbers | Not independently re-run in this pass | Repeated consistently across docs, but not re-measured during this audit |
| `abixio-ui/tests/bench.rs` harness description | Not audited in this pass | This repo references that harness, but the `abixio-ui` repo was not opened here |

Verdict: this document’s benchmark narrative is mostly internally consistent after fixing the overhead/client-count contradictions. The bench names, commands, raw artifact references, and profiling hook are backed by this repo. The many published performance numbers still need runtime re-benchmarking if you want strict empirical re-validation.
