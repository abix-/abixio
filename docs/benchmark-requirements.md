# Benchmark Requirements

What we test, why, and how. This is the spec. All benchmark code
must satisfy these requirements.

## Repo ownership

All benchmarks live in abixio-ui. One harness, one place.

Test files: `abixio-ui/src/bench/`

abixio-ui depends on the abixio crate as a library, so it can
directly import and call internal APIs (VolumePool, LocalVolume,
Backend, ClusterManager, AbixioDispatch, etc.) for in-process
layer testing. It also spawns real server processes for L7 and
competitive comparison.

| Layer | How it runs |
|---|---|
| L1 (HTTP transport) | in-process hyper server |
| L2 (S3 protocol) | in-process s3s server |
| L3 (VolumePool) | in-process, direct VolumePool API calls |
| L4 (hashing + RS) | in-process, direct abixio API calls |
| L5 (disk I/O) | in-process, direct tokio::fs calls |
| L6 (S3 + real storage) | in-process s3s + real VolumePool |
| L7 (full SDK client) | child process, real TCP/TLS |
| Competitive comparison | child processes (AbixIO, RustFS, MinIO) |

L3, L6, and L7 include real storage, so they test all write path
configurations (3 tiers x 2 cache states).

L7 and competitive comparison spawn real server binaries. Everything
else calls abixio library code directly.

### File layout

```
src/bench/
    mod.rs              -- run(), CLI arg routing
    stats.rs            -- BenchResult, Stats, print/parse, JSON output, baseline comparison
    l1_http.rs          -- hyper transport floor (PUT/GET, no S3)
    l2_s3proto.rs       -- s3s protocol overhead (NullBackend, no real storage)
    l3_storage.rs       -- VolumePool put/get/put_stream/get_stream, 1+4 disks
    l3_pool_internals.rs -- pool write path internals (slot primitives, write strategies,
                           JSON serializers, rename worker, integrated write_shard, breakdowns)
    l4_compute.rs       -- hashing (blake3, md5, sha256) + reed-solomon encode
    l5_disk.rs          -- raw disk I/O (write, write+fsync, read)
    l6_s3storage.rs     -- s3s + real VolumePool (write path x cache matrix)
    l6_stack_breakdown.rs -- 5-stage latency attribution (hyper -> s3s -> file -> pool)
    l7_e2e.rs           -- full SDK client, child servers, competitive comparison
    tls.rs              -- TLS cert generation for HTTPS benchmarks
    servers.rs          -- AbixioServer builder, ExternalServer (RustFS/MinIO)
    clients.rs          -- AwsCliHarness, rclone helpers, binary finders
```

### What each layer tests

| Layer | File | What it measures | Write path varies? |
|---|---|---|---|
| L1 | l1_http.rs | hyper PUT/GET, no S3 | no |
| L2 | l2_s3proto.rs | s3s SigV4+XML, NullBackend | no |
| L3 | l3_storage.rs | VolumePool put/get, streaming, 1+4 disks | yes (file/log/pool x cache on/off) |
| L3 pool | l3_pool_internals.rs | slot acquire/release, write strategies, JSON serializers, rename worker drain, integrated write_shard, per-step breakdowns | pool only |
| L4 | l4_compute.rs | blake3, md5, sha256, reed-solomon encode | no |
| L5 | l5_disk.rs | tokio::fs write/read, fsync | no |
| L6 | l6_s3storage.rs | s3s + real VolumePool | yes (file/log/pool x cache on/off) |
| L6 stack | l6_stack_breakdown.rs | 5-stage attribution at 4KB (A through E variants) | file + pool |
| L7 | l7_e2e.rs | full SDK/aws-cli/rclone, AbixIO/RustFS/MinIO, all ops | yes (file/log/pool x cache on/off) |

## Design principle

One configurable harness, not dozens of separate tests.
Every axis is selectable: run all benchmarks or narrow to exactly
what you need.

## Configuration

All axes controlled by CLI flags. Default: run everything.
Comma-separated values to select multiple. Single value to narrow.

| Flag | Values | Default |
|---|---|---|
| `--sizes` | `4KB,64KB,10MB,100MB,1GB` | all |
| `--layers` | `L1,L2,L3,L4,L5,L6,L7` | all |
| `--write-paths` | `file,log,pool` | all |
| `--write-cache` | `on,off,both` | both |
| `--servers` | `abixio,rustfs,minio` | all |
| `--clients` | `sdk,aws-cli,rclone` | all |
| `--ops` | `PUT,GET,HEAD,LIST,DELETE` | all |
| `--iters` | number | auto-scaled by size |
| `--tls` | `on,off,both` | on |
| `--tmp-dir` | path | system temp dir |

Examples:

```bash
# full suite
abixio-ui bench

# just 4KB PUT through the pool write path, no write cache
abixio-ui bench --sizes 4KB --ops PUT --write-paths pool --write-cache off

# just the competitive comparison at 10MB
abixio-ui bench --sizes 10MB --layers L7

# just the disk baseline
abixio-ui bench --layers L5

# write cache on vs off for all tiers at 4KB
abixio-ui bench --sizes 4KB --write-cache both
```

## Sizes

All benchmarks must support these sizes:

| Size | Purpose |
|---|---|
| 4KB | small object hot path (metadata-bound) |
| 64KB | mid-small, tests transition between small/large paths |
| 10MB | medium, throughput starts to matter |
| 100MB | large, sustained throughput |
| 1GB | very large, streaming path, memory pressure |

## Requirement 1: Raw disk I/O baseline

Measure the filesystem floor. Nothing in the stack can exceed this.

- `tokio::fs::write` (page cache, may not hit disk)
- `tokio::fs::write` + `fsync` (forces to physical disk)
- `tokio::fs::read`
- At every size

This is Layer 5 (L5). It answers: "how fast is the disk?"

## Requirement 2: Per-layer performance through the stack

Each layer must be tested in isolation. The benchmark for a layer
measures ONLY that layer's overhead, not the layers above or below it.
This is critical for tuning: if you cannot measure a layer by itself,
you cannot tell whether a change to that layer made it faster or slower.

### Isolation rules

- L1 tests ONLY HTTP transport. No S3, no storage, no compute.
- L2 tests ONLY S3 protocol parsing and dispatch. No TCP/HTTP
  transport overhead. Uses in-memory pipe, not a TCP socket.
- L3 tests ONLY the storage pipeline. Calls VolumePool directly,
  no HTTP, no s3s.
- L4 tests ONLY hashing and erasure coding. Direct function calls,
  no I/O.
- L5 tests ONLY raw disk I/O. Direct filesystem calls, no storage
  pipeline.
- L6 is NOT isolated. It is an integration test that combines
  L1+L2+L3 into a single in-process stack (s3s + real VolumePool).
- L7 is NOT isolated. It is a full end-to-end test with a real
  server process, real client, TLS, and auth.

L1 through L5 are isolated layers. Each one can be tuned independently.
L6 and L7 are integration layers that show how the isolated layers
compose together.

### Layer table

| Layer | What it measures | Isolation method | Write path varies? |
|---|---|---|---|
| L1 | HTTP transport | bare hyper, reqwest client, no S3 | no |
| L2 | S3 protocol | s3s dispatch via in-memory pipe, NullBackend | no |
| L3 | storage pipeline | direct VolumePool API, no HTTP | yes (file/log/pool x cache on/off) |
| L4 | hashing + erasure coding | direct function calls | no |
| L5 | raw disk I/O | direct tokio::fs calls | no |
| L6 | S3 + real storage (integration) | in-process s3s + VolumePool | yes (file/log/pool x cache on/off) |
| L7 | full e2e (integration) | child process server, real SDK client | yes (file/log/pool x cache on/off) |

Each layer tested at every size.

L3, L6, and L7 include real storage, so they must be tested across
all write path configurations (3 tiers x 2 cache states = 6 configs).
L1, L2, L4, L5 have no storage, so write path does not apply.

## Requirement 3: Per-write-path performance

Two independent axes: write tier and write cache. Test every combination.

### Write tiers

| Tier | What it is | CLI flag |
|---|---|---|
| file | baseline, direct filesystem writes | `--write-tier file` |
| log | log-structured append (small object optimization) | `--write-tier log` |
| pool | pre-opened temp file pool + async rename workers | `--write-tier pool` |

### Write cache

| State | What it is | CLI flag |
|---|---|---|
| off | no RAM cache, writes go directly to the tier | `--no-write-cache` |
| on | RAM DashMap, immediate ack, async flush to disk | (default, no flag needed) |

### Test matrix

Every tier tested with write cache off AND write cache on:

| Config | CLI flags |
|---|---|
| file | `--write-tier file --no-write-cache` |
| file+wc | `--write-tier file` |
| log | `--write-tier log --no-write-cache` |
| log+wc | `--write-tier log` |
| pool | `--write-tier pool --no-write-cache` |
| pool+wc | `--write-tier pool` |

This produces 6 AbixIO configurations. The raw tier rows (without
write cache) show true disk-tier performance. The +wc rows show the
write cache benefit on top of each tier.

All operations (PUT, GET, HEAD, LIST, DELETE) tested per configuration.
All sizes tested per configuration.

## Requirement 4: Competitive comparison

AbixIO vs RustFS vs MinIO through real S3 clients.

| Axis | Values |
|---|---|
| Servers | AbixIO (each write path), RustFS, MinIO |
| Clients | aws-sdk-s3, aws-cli, rclone |
| Operations | PUT, GET, HEAD, LIST, DELETE |
| Sizes | 4KB, 64KB, 10MB, 100MB, 1GB |
| Auth | HTTPS + SigV4 + UNSIGNED-PAYLOAD |

All servers run single-node, 1 disk, NTFS tmpdir, same machine,
release build. Servers started fresh for each configuration.

HEAD, LIST, DELETE are latency-bound metadata ops. These only need
the sdk client (CLI process spawn overhead makes the comparison
meaningless for sub-millisecond ops).

### Test infrastructure

All benchmarks launch real server processes, create temp dirs, and
benchmark through real S3 clients. No synthetic tests, no mocked
storage. Real servers, real clients, real data.

### Clients

| Client | Type | Auth | Connection | Overhead |
|---|---|---|---|---|
| aws-sdk-s3 (Rust) | in-process SDK | SigV4, UNSIGNED-PAYLOAD | keep-alive | none |
| AWS CLI | per-process CLI | SigV4, UNSIGNED-PAYLOAD via `payload_signing_enabled = false` | new process per op | measured at runtime |
| rclone | per-process CLI | SigV4, UNSIGNED-PAYLOAD via `--s3-use-unsigned-payload true` | new process per op | measured at runtime |

For CLI tools, process spawn overhead is measured before each benchmark
run and printed in the output for transparency.

### Servers

- AbixIO: Rust S3 server (this project)
- RustFS 1.0.0-alpha.90: Rust S3 server (MinIO-compatible)
- MinIO RELEASE.2026-04-07: Go S3 server (reference implementation)

All binaries must be release builds. Debug builds are 5-7x slower.
RustFS and MinIO binaries auto-detected at `C:\tools\rustfs.exe` and
`C:\tools\minio.exe`. Override with `RUSTFS_BIN` and `MINIO_BIN` env vars.

### Metrics

- obj/sec: operations per second (primary metric for small objects)
- MB/s: throughput (primary metric for large objects)
- latency: per-request time in microseconds or milliseconds

## Requirement 5: Fairness

Authoritative normalized client mode: HTTPS + SigV4 + UNSIGNED-PAYLOAD.
Cross-client numbers are only directly comparable when the harness holds
the major variables constant.

1. **Same warmup.** 3 PUT + 3 GET warmup operations before timing, for
   every client. Ensures TCP connections are established, caches are warm,
   and JIT (if any) has run.

2. **Same I/O model.** All clients read PUT payload from a temp file on
   disk. All clients write GET output to a temp file on disk. No client
   gets the advantage of in-memory I/O while others do disk I/O.

3. **Same connection warming.** Every client gets the same warmup count
   before timing. Connection reuse is not fully normalized: aws-sdk-s3
   runs in-process and reuses connections, while AWS CLI and rclone are
   invoked as new processes per operation. Published matrix numbers are
   end-to-end and include that difference.

4. **Same iterations.** All clients run the same number of iterations per
   (server, size) combination.

5. **Same payload.** Identical byte pattern, identical size, identical
   content-type for all clients.

6. **Same auth mode.** HTTPS + SigV4 + UNSIGNED-PAYLOAD for every
   comparable client: aws-sdk-s3, AWS CLI, and rclone.

7. **Same server config.** Each server runs single-node, 1 disk, NTFS
   tmpdir, same machine, release build. Servers are started fresh for
   each benchmark run.

## Requirement 6: Reproducibility

- All benchmarks must report: p50 (median), p95 (tail), p99 (worst case), ops/sec, MB/s (where applicable)
- CLI process spawn overhead must be measured and reported (not subtracted)
- Results must include git commit hash and timestamp
- Results must be machine-readable (JSON output alongside human tables)

## Known limitations

- Windows-only: all benchmarks run on Windows 10. Linux numbers may
  differ due to epoll vs IOCP, different TCP stack, different filesystem
  performance.
- Single machine: client and server share CPU, memory, and network stack.
  No network latency. Results represent localhost throughput, not
  production deployment.
- Per-process CLI overhead: uses a lightweight operation (ls/lsd) to
  estimate overhead. Actual cp process overhead may be slightly higher.
  Small objects may show inflated throughput after subtraction.
- Iteration count: 3 iterations for 1GB may show variance. Larger
  iteration counts are more reliable for large-object numbers.

## Windows caveats

- Always use `127.0.0.1`, never `localhost` (Windows DNS adds ~200ms)
- TCP connect on Windows loopback = ~0.2ms (Linux = ~0.03ms)
- TCP_NODELAY must be set explicitly (Go sets it by default)
- hyper needs `writev(true)` + `max_buf_size(4MB)` for optimal throughput

## What we do NOT test

- Multi-disk EC scaling (2/3/4 disks). Separate concern, not part of the core benchmark.
- Concurrent/parallel clients. Sequential only for now.
- Network latency. Localhost only.
- Linux. Windows 10 only. Document this limitation.

## Running benchmarks

```bash
# build
cd /path/to/abixio-ui && cargo build --release

# full suite
abixio-ui bench

# just one layer
abixio-ui bench --layers L3

# just competitive comparison
abixio-ui bench --layers L7 --servers abixio,rustfs,minio
```
