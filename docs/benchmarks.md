# Benchmarks

Real S3 operations through the full stack: aws-sdk-s3 -> HTTP -> hyper -> s3s ->
VolumePool -> erasure encode -> LocalVolume -> tokio::fs -> disk.

## Test setup

- Windows 10, single machine
- All volumes on same NTFS drive (tmpdir)
- Single-node, local disk only
- aws-sdk-s3 client with SigV4 authentication
- No concurrent requests (sequential, single client)
- Parallel shard I/O via `futures::join_all`

## Results (2026-04-07, release build)

### 1 disk (EC 1+0, no parity)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 6.3ms | 4.4ms | 20.9ms | 160 KB/s |
| PUT | 1MB | 20 | 95ms | 83ms | 130ms | 10.5 MB/s |
| PUT | 10MB | 5 | 771ms | 725ms | 769ms | 13.0 MB/s |
| GET | 1KB | 100 | 2.8ms | 2.7ms | 4.8ms | 352 KB/s |
| GET | 1MB | 20 | 9.5ms | 9.2ms | 10.7ms | 106 MB/s |
| GET | 10MB | 5 | 86ms | 74ms | 77ms | 116 MB/s |
| HEAD | - | 100 | 2.5ms | 2.4ms | 5.6ms | - |
| LIST | 100 obj | 50 | 20.5ms | 20.1ms | 24.3ms | - |
| DELETE | 1KB | 100 | 2.7ms | 2.7ms | 3.4ms | - |

### 2 disks (EC 1+1)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 8.3ms | 5.2ms | 26.8ms | 120 KB/s |
| PUT | 1MB | 20 | 95ms | 89ms | 118ms | 10.5 MB/s |
| PUT | 10MB | 5 | 798ms | 767ms | 823ms | 12.5 MB/s |
| GET | 1KB | 100 | 3.2ms | 3.1ms | 4.6ms | 310 KB/s |
| GET | 1MB | 20 | 14ms | 14ms | 16ms | 72 MB/s |
| GET | 10MB | 5 | 116ms | 111ms | 119ms | 86 MB/s |
| HEAD | - | 100 | 2.3ms | 2.2ms | 4.8ms | - |
| LIST | 100 obj | 50 | 25ms | 24ms | 28ms | - |
| DELETE | 1KB | 100 | 3.3ms | 3.2ms | 4.2ms | - |

### 3 disks (EC 2+1)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 9.2ms | 5.9ms | 36.3ms | 108 KB/s |
| PUT | 1MB | 20 | 96ms | 89ms | 140ms | 10.5 MB/s |
| PUT | 10MB | 5 | 737ms | 722ms | 749ms | 13.6 MB/s |
| GET | 1KB | 100 | 3.2ms | 3.0ms | 4.7ms | 317 KB/s |
| GET | 1MB | 20 | 13ms | 12ms | 18ms | 78 MB/s |
| GET | 10MB | 5 | 98ms | 93ms | 96ms | 102 MB/s |
| HEAD | - | 100 | 2.3ms | 2.2ms | 3.3ms | - |
| LIST | 100 obj | 50 | 28ms | 27ms | 34ms | - |
| DELETE | 1KB | 100 | 3.9ms | 3.8ms | 4.6ms | - |

### 4 disks (EC 3+1)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 10.5ms | 7.4ms | 32.8ms | 95 KB/s |
| PUT | 1MB | 20 | 88ms | 89ms | 96ms | 11.3 MB/s |
| PUT | 10MB | 5 | 729ms | 718ms | 746ms | 13.7 MB/s |
| GET | 1KB | 100 | 3.2ms | 3.0ms | 4.7ms | 317 KB/s |
| GET | 1MB | 20 | 13ms | 12ms | 15ms | 79 MB/s |
| GET | 10MB | 5 | 99ms | 93ms | 103ms | 101 MB/s |
| HEAD | - | 100 | 2.4ms | 2.2ms | 3.6ms | - |
| LIST | 100 obj | 50 | 31ms | 30ms | 36ms | - |
| DELETE | 1KB | 100 | 4.5ms | 4.4ms | 6.3ms | - |

## Observations

**GET throughput is excellent.** 116 MB/s on 1 disk, 101 MB/s on 4 disks. The
bottleneck is now disk I/O, not CPU. Reed-Solomon decode only reads `data_n`
shards (not parity), so the overhead is minimal.

**PUT throughput is disk-bound.** ~13 MB/s across all disk counts. All volumes are
on the same physical NTFS drive, so parallel shard writes don't help much. With
separate physical disks, PUT throughput should scale linearly with disk count.

**PUT latency scales with disk count for small objects.** 4.4ms (1 disk) to 7.4ms
(4 disks) p50 for 1KB. More shards = more metadata writes.

**HEAD is consistently fast.** ~2.2ms p50 regardless of disk count.

**Parallel I/O matters.** Shard reads and writes use `futures::join_all` to issue
all disk operations concurrently. On separate physical disks, this is the
difference between N sequential writes and 1 parallel write.

## Improvement from v1 (debug, sequential) to v2 (release, parallel)

| Metric | v1 | v2 | Speedup |
|---|---|---|---|
| GET 10MB, 1 disk | 16.7 MB/s | 116 MB/s | 7x |
| GET 10MB, 4 disks | 11.4 MB/s | 101 MB/s | 9x |
| GET 1MB, 4 disks | 12.2 MB/s | 79 MB/s | 6.5x |
| PUT 10MB, 4 disks | 4.4 MB/s | 13.7 MB/s | 3x |
| PUT 1MB, 4 disks | 4.1 MB/s | 11.3 MB/s | 2.8x |

Main contributors: release-mode optimized crypto (SHA256, MD5, Reed-Solomon) and
parallel shard I/O via join_all.

## Running benchmarks

Release binary (recommended):
```
cd abixio && cargo build --release
cd abixio-ui
ABIXIO_BIN='C:\code\endless\rust\target\release\abixio.exe' cargo test --test bench -- --ignored --nocapture
```

Debug binary (for comparison):
```
cd abixio-ui
cargo test --test bench -- --ignored --nocapture
```

Single config:
```
cargo test --test bench -- --ignored --nocapture bench_4_disks
```
