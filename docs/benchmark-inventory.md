# Benchmark Inventory

All benchmarks live in `abixio-ui/src/bench/`. One harness, one place.
Run via `abixio-ui bench` with CLI flags to select what to run.

Last updated: 2026-04-12.

---

## File layout

```
src/bench/
    mod.rs              -- run(), CLI arg routing
    stats.rs            -- BenchResult, Stats, print/parse, JSON output, baseline comparison
    l1_disk.rs          -- raw disk I/O (write, write+fsync, read)
    l2_compute.rs       -- hashing (blake3, md5, sha256) + reed-solomon encode
    l3_storage.rs       -- VolumePool put/get/put_stream/get_stream, 1+4 disks
    l3_pool_internals.rs -- pool write path internals (slot primitives, write strategies,
                           JSON serializers, rename worker, integrated write_shard, breakdowns)
    l4_http.rs          -- hyper transport floor (PUT/GET, no S3)
    l5_s3proto.rs       -- s3s protocol overhead (NullBackend, no real storage)
    l6_s3storage.rs     -- s3s + real VolumePool (write path x cache matrix)
    l6_stack_breakdown.rs -- 5-stage latency attribution (hyper -> s3s -> file -> pool)
    l7_e2e.rs           -- full SDK client, child servers, competitive comparison
    tls.rs              -- TLS cert generation for HTTPS benchmarks
    servers.rs          -- AbixioServer builder, ExternalServer (RustFS/MinIO)
    clients.rs          -- AwsCliHarness, rclone helpers, binary finders
```

## What each layer tests

| Layer | File | What it measures | Write path varies? |
|---|---|---|---|
| L1 | l1_disk.rs | tokio::fs write/read, fsync | no |
| L2 | l2_compute.rs | blake3, md5, sha256, reed-solomon encode | no |
| L3 | l3_storage.rs | VolumePool put/get, streaming, 1+4 disks | yes (file/log/pool x cache on/off) |
| L3 pool | l3_pool_internals.rs | slot acquire/release, write strategies, JSON serializers, rename worker drain, integrated write_shard, per-step breakdowns | pool only |
| L4 | l4_http.rs | hyper PUT/GET, no S3 | no |
| L5 | l5_s3proto.rs | s3s SigV4+XML, NullBackend | no |
| L6 | l6_s3storage.rs | s3s + real VolumePool | yes (file/log/pool x cache on/off) |
| L6 stack | l6_stack_breakdown.rs | 5-stage attribution at 4KB (A through E variants) | file + pool |
| L7 | l7_e2e.rs | full SDK/aws-cli/rclone, AbixIO/RustFS/MinIO, all ops | yes (file/log/pool x cache on/off) |

## CLI flags

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
| `--output` | path | none (no JSON saved) |
| `--baseline` | path | none (no comparison) |

