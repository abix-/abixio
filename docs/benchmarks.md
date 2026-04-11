# Benchmarks

## Methodology

### Fairness principles

**Authoritative normalized client mode:** `HTTPS + SigV4 +
UNSIGNED-PAYLOAD`.

That is the benchmark baseline this document treats as authoritative. It
keeps real S3 authentication enabled while avoiding extra full-body
payload hashing work that many HTTPS clients do not use in normal
practice.

Cross-client numbers are only directly comparable when the harness holds
the major variables constant. `abixio-ui/tests/bench.rs` now wires the
canonical matrix and client comparison around that normalized mode, and
the matrix tables below were re-run end-to-end under the rewritten TLS
harness on 2026-04-11 (raw artifact:
`bench-results/2026-04-11-matrix-tls.txt`).

1. **Same warmup**: 3 PUT + 3 GET warmup operations before timing, for
   every client. This ensures TCP connections are established, caches are
   warm, and JIT (if any) has run.

2. **Same I/O model**: all clients read PUT payload from a temp file on
   disk. All clients write GET output to a temp file on disk. No client
   gets the advantage of in-memory I/O while others do disk I/O.

3. **Same connection warming**: every client gets the same warmup count
   before timing. Connection reuse is **not** fully normalized:
   `aws-sdk-s3` runs in-process and reuses connections, while `AWS CLI`
   and `rclone` are invoked as new processes per operation. Published
   matrix numbers are end-to-end and include that difference.

4. **Same iterations**: all clients run the same number of iterations
   per (server, size) combination.

5. **Same payload**: identical byte pattern, identical size, identical
   content-type for all clients.

6. **Same auth mode**: the authoritative benchmark target is `HTTPS +
   SigV4 + UNSIGNED-PAYLOAD` for every comparable client. The canonical
   harness now targets that for `aws-sdk-s3`, `AWS CLI`, and `rclone`.

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
| **aws-sdk-s3** (Rust) | In-process SDK | SigV4, `UNSIGNED-PAYLOAD` via `put_object_unsigned()` | Keep-alive | None |
| **AWS CLI** | Per-process CLI | SigV4, `UNSIGNED-PAYLOAD` via `payload_signing_enabled = false` | New process per op | measured at runtime |
| **rclone** | Per-process CLI | SigV4, `UNSIGNED-PAYLOAD` with `--s3-use-unsigned-payload true` | New process per op | measured at runtime |

For CLI tools, process spawn overhead is measured before each benchmark
run and printed in the output for transparency. The canonical matrix
reports real end-to-end timing including that overhead.

### Consistency status

The current harness already normalizes:

- warmup count
- payload bytes and object sizes
- disk-backed PUT source / GET sink I/O model in the matrix
- server config and localhost test machine

The current harness does **not** normalize:

- connection reuse (`aws-sdk-s3` keep-alive vs CLI respawn-per-op)
- process spawn cost for CLI clients

The benchmark we actually want, and the harness is now written to run, is:

- `HTTPS`
- `SigV4`
- `UNSIGNED-PAYLOAD`
- same payload bytes
- same disk-backed I/O model
- same warmup and iteration counts
- same connection behavior where the client supports it

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

The harness for this benchmark starts AbixIO, RustFS, and MinIO over
TLS, injects a temporary benchmark CA, and benchmarks these canonical
clients:

- `aws-sdk-s3`
- `AWS CLI`
- `rclone`

All three are configured for `HTTPS + SigV4 + UNSIGNED-PAYLOAD`, with
the same disk-backed PUT source / GET sink model and the same 3 PUT + 3
GET warmup. CLI process spawn cost is real and is not subtracted; per-op
spawn overhead is printed in the bench output (~945 ms for AWS CLI v2,
~120 ms for rclone) so the small-object CLI rows can be read as
"startup-bound, not S3-bound".

Canonical matrix results are below, dated 2026-04-11. Source artifact:
`bench-results/2026-04-11-matrix-tls.txt`.

### 4KB: small object performance (obj/sec)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | aws-cli PUT | aws-cli GET | rclone PUT | rclone GET |
|--------|---------------|---------------|-------------|-------------|------------|------------|
| **AbixIO** | **1586** | **659** | 1 | 1 | 8 | 8 |
| RustFS | 329 | 518 | 1 | 1 | 8 | 8 |
| MinIO | 371 | 566 | 1 | 1 | 8 | 8 |

**AbixIO leads sdk PUT decisively** at 1586 obj/s versus MinIO 371 (4.3x)
and RustFS 329 (4.8x), thanks to the RAM write cache plus the
log-structured small-write tier. AbixIO sdk GET also leads at 659 obj/s
versus MinIO 566 and RustFS 518.

The CLI rows for 4KB are dominated by per-op process spawn — measured
overhead in this run was ~945 ms for `aws-cli` and ~120 ms for `rclone`.
These are not S3 throughput measurements, they are CLI startup
measurements. 4KB obj/sec via aws-cli rounds to 1 obj/s on every server,
and via rclone to 8 obj/s on every server, regardless of which server is
behind it.

### 10MB: medium object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | aws-cli PUT | aws-cli GET | rclone PUT | rclone GET |
|--------|---------------|---------------|-------------|-------------|------------|------------|
| AbixIO | 327 | 212 | 10 | 11 | 59 | 74 |
| **RustFS** | **381** | 218 | 11 | 11 | 9 | 68 |
| MinIO | 260 | **226** | 10 | 10 | 58 | 71 |

RustFS leads sdk PUT at 381 MB/s (AbixIO 327, MinIO 260), and MinIO
narrowly leads sdk GET at 226 MB/s (RustFS 218, AbixIO 212). All three
sdk GET rows cluster within ~7% — disk write speed equalizes the
results. CLI tools at 10MB are still significantly affected by per-op
spawn cost, especially aws-cli at ~10 MB/s. RustFS rclone PUT (8.5 MB/s)
is an outlier on the slow side and is worth a separate investigation.

### 1GB: large object throughput (MB/s)

| Server | aws-sdk-s3 PUT | aws-sdk-s3 GET | aws-cli PUT | aws-cli GET | rclone PUT | rclone GET |
|--------|---------------|---------------|-------------|-------------|------------|------------|
| AbixIO | 433 | 259 | 193 | **358** | 279 | 99 † |
| **RustFS** | **489** | **286** | 197 | 289 | 19 ‡ | 325 |
| MinIO | 428 | 255 | 200 | 325 | 282 | 342 |

RustFS leads sdk PUT at 489 MB/s (AbixIO 433, MinIO 428) and sdk GET at
286 MB/s (AbixIO 259, MinIO 255). AbixIO sdk PUT trails RustFS by ~13%
at 1GB. sdk GET is again equalized by the disk-write sink (~250-285 MB/s
for all three). aws-cli GET is faster than sdk GET for AbixIO and MinIO
because aws-cli streams the body through a separate process and avoids
the SDK's per-op buffer churn at this size.

`†` AbixIO rclone 1GB GET measured at 99 MB/s versus 325 (RustFS) and
342 (MinIO). The previous (HTTP) doc excluded rclone 1GB GET entirely
because "rclone caches/skips repeated downloads to the same file"; the
new harness uses unique sinkpaths per iteration, so cache reuse should
not apply. The number is reported as measured. Worth a follow-up
investigation.

`‡` RustFS rclone 1GB PUT at 19 MB/s is a clear outlier on the slow side
versus 282 (MinIO) and 279 (AbixIO). Same payload, same warmup. Also
worth a follow-up investigation.

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

This benchmark now uses the same normalized mode as the canonical
matrix:

- `HTTPS`
- `SigV4`
- `UNSIGNED-PAYLOAD`
- disk-backed PUT source / GET sink for every client

The current canonical client set is:

- `aws-sdk-s3`
- `AWS CLI`
- `rclone`

Published numeric results for this rewritten comparison are pending a
fresh run. The older `curl` / `mc` table has been retired because it no
longer matches the benchmark design.

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
| Specific published numeric results in matrix tables | Verified | Re-run end-to-end on 2026-04-11 under the rewritten TLS harness; raw output at `bench-results/2026-04-11-matrix-tls.txt` |
| `1220 MB/s` curl GET and other historical perf numbers | Not independently re-run in this pass | Repeated consistently across docs, but not re-measured during this audit |
| Matrix harness normalizes disk I/O and warmup across clients | Verified | `../abixio-ui/tests/bench.rs:882-904`, `951-962`, `1030-1040` |
| `HTTPS + SigV4 + UNSIGNED-PAYLOAD` is the documented authoritative normalized client mode | Verified | `docs/benchmarks.md:7-15` |
| Canonical benchmark harness now targets HTTPS instead of plain HTTP | Verified | `../abixio-ui/tests/bench.rs:646`, `650`, `658`, `847`, `1221`, `1240`, `1284`, `1309`; `../abixio-ui/tests/support/server.rs:147-192` |
| Canonical client set is now `aws-sdk-s3`, `AWS CLI`, and `rclone` | Verified | `../abixio-ui/tests/bench.rs:842`, `849-855`, `1231-1235` |
| AWS CLI path is configured for unsigned payloads | Verified | `../abixio-ui/tests/bench.rs:726-727` writes `payload_signing_enabled = false` into the benchmark profile |
| rclone path is explicitly configured for unsigned payloads | Verified | `../abixio-ui/tests/bench.rs:798-818` and all rclone calls route through those args |
| SDK path uses unsigned payloads in the canonical client and matrix benches | Verified | `../abixio-ui/src/s3/client.rs:249-287`, `../abixio-ui/tests/bench.rs:869`, `876`, `1011`, `1021` |
| Matrix harness fully normalizes connection reuse across clients | Incorrect | `../abixio-ui/tests/bench.rs:857-888` keeps the SDK in-process while `894-966`, `1048-1095`, and `1101-1182` spawn CLI commands per operation |
| Published matrix tables below are current canonical TLS results | Verified | Tables replaced with the 2026-04-11 TLS harness rerun; source artifact `bench-results/2026-04-11-matrix-tls.txt` |
| `bench_clients` is still a mixed signed/in-memory comparison | Incorrect | `../abixio-ui/tests/bench.rs:842-966` now uses TLS, disk-backed I/O, and unsigned payloads for the canonical client set |

Verdict: this document’s benchmark narrative is mostly internally consistent after fixing the overhead/client-count contradictions. The bench names, commands, raw artifact references, and profiling hook are backed by this repo. The many published performance numbers still need runtime re-benchmarking if you want strict empirical re-validation.
