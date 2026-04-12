# Benchmark Requirements

What we test, why, and how. This is the spec. All benchmark code
must satisfy these requirements.

## Design principle

One configurable harness, not dozens of separate tests. Every axis
is selectable: run all benchmarks or narrow to exactly what you need.

## Configuration

All axes are controlled by env vars. Default: run everything.

| Env var | Values | Default |
|---|---|---|
| `BENCH_SIZES` | `4KB,64KB,10MB,100MB,1GB` | all |
| `BENCH_LAYERS` | `L1,L2,L3,L4,L5,L6,L7` | all |
| `BENCH_WRITE_PATHS` | `file,log,pool,ram` | all |
| `BENCH_SERVERS` | `abixio,rustfs,minio` | all |
| `BENCH_CLIENTS` | `sdk,aws-cli,rclone` | all |
| `BENCH_OPS` | `PUT,GET,HEAD,LIST,DELETE` | all |

Examples:

```bash
# full suite
k3sc cargo-lock test --release --test bench -- --ignored --nocapture bench_all

# just 4KB PUT through the pool write path
BENCH_SIZES=4KB BENCH_OPS=PUT BENCH_WRITE_PATHS=pool bench_all

# just the competitive comparison at 10MB
BENCH_SIZES=10MB BENCH_LAYERS=L7 bench_all

# just the disk baseline
BENCH_LAYERS=L1 bench_all
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

This is Layer 1 (L1). It answers: "how fast is the disk?"

## Requirement 2: Per-layer performance through the stack

Measure each layer independently so we can attribute latency.

| Layer | What it measures | Includes |
|---|---|---|
| L1 | raw disk I/O | tokio::fs write/read, fsync |
| L2 | hashing + erasure coding | blake3, md5, reed-solomon encode |
| L3 | storage pipeline | VolumePool put/get, no HTTP |
| L4 | HTTP transport | hyper round-trip, no S3 protocol |
| L5 | S3 protocol | s3s SigV4 + XML parsing, no real storage |
| L6 | S3 protocol + real storage | s3s + VolumePool, no SDK |
| L7 | full SDK client | aws-sdk-s3 end-to-end |

Each layer tested at every size. Subtracting consecutive layers
attributes latency to each component.

## Requirement 3: Per-write-path performance

Each write path tested independently through the full stack (L7).

| Write path | What it is | CLI flag |
|---|---|---|
| file | baseline, direct filesystem writes | `--write-tier file --no-write-cache` |
| log | log-structured append (small object optimization) | `--write-tier log --no-write-cache` |
| pool | pre-opened temp file pool + async rename workers | `--write-tier pool --no-write-cache` |
| ram | RAM write cache (DashMap, immediate ack, async flush) | `--write-tier file` (write cache on by default) |

The `ram` path is the write cache layered on top of the file tier.
Each path must be testable in isolation. The `--no-write-cache` flag
controls whether the RAM cache is active.

All operations (PUT, GET, HEAD, LIST, DELETE) tested per write path.
All sizes tested per write path.

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

## Requirement 5: Reproducibility

- All benchmarks must warmup before timing (3 PUT + 3 GET minimum)
- All benchmarks must report: avg, p50, p99, ops/sec, MB/s (where applicable)
- All benchmarks must use the same payload bytes and content-type
- CLI process spawn overhead must be measured and reported (not subtracted)
- Results must include git commit hash and timestamp
- Results must be machine-readable (JSON output alongside human tables)

## What we do NOT test

- Multi-disk EC scaling (2/3/4 disks) -- separate concern, not part of the core benchmark
- Concurrent/parallel clients -- sequential only for now
- Network latency -- localhost only
- Linux -- Windows 10 only (document this limitation)
