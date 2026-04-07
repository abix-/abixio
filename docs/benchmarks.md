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
- Non-blocking disk I/O via `tokio::fs`

## Results (2026-04-07, release build, tokio::fs)

### 1 disk (EC 1+0, no parity)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 6.6ms | 5.7ms | 22.4ms | 152 KB/s |
| PUT | 1MB | 20 | 106ms | 92ms | 184ms | 9.4 MB/s |
| PUT | 10MB | 5 | 809ms | 734ms | 820ms | 12.4 MB/s |
| GET | 1KB | 100 | 3.4ms | 3.1ms | 8.8ms | 293 KB/s |
| GET | 1MB | 20 | 9.9ms | 9.4ms | 11.8ms | 101 MB/s |
| GET | 10MB | 5 | 79ms | 70ms | 89ms | 127 MB/s |
| HEAD | - | 100 | 2.9ms | 2.4ms | 7.9ms | - |
| LIST | 100 obj | 50 | 26.3ms | 26.1ms | 28.4ms | - |
| DELETE | 1KB | 100 | 3.1ms | 2.8ms | 11.2ms | - |

### 2 disks (EC 1+1)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 9.5ms | 8.0ms | 26.7ms | 105 KB/s |
| PUT | 1MB | 20 | 104ms | 94ms | 149ms | 9.7 MB/s |
| PUT | 10MB | 5 | 811ms | 760ms | 859ms | 12.3 MB/s |
| GET | 1KB | 100 | 3.3ms | 3.2ms | 4.4ms | 307 KB/s |
| GET | 1MB | 20 | 14ms | 14ms | 17ms | 70 MB/s |
| GET | 10MB | 5 | 128ms | 118ms | 129ms | 78 MB/s |
| HEAD | - | 100 | 2.3ms | 2.3ms | 3.0ms | - |
| LIST | 100 obj | 50 | 31ms | 30ms | 43ms | - |
| DELETE | 1KB | 100 | 3.6ms | 3.6ms | 4.5ms | - |

### 3 disks (EC 2+1)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 11.3ms | 10.4ms | 25.5ms | 88 KB/s |
| PUT | 1MB | 20 | 95ms | 93ms | 112ms | 10.5 MB/s |
| PUT | 10MB | 5 | 775ms | 744ms | 791ms | 12.9 MB/s |
| GET | 1KB | 100 | 3.6ms | 3.2ms | 9.0ms | 280 KB/s |
| GET | 1MB | 20 | 12ms | 12ms | 14ms | 83 MB/s |
| GET | 10MB | 5 | 95ms | 92ms | 96ms | 106 MB/s |
| HEAD | - | 100 | 2.3ms | 2.3ms | 3.3ms | - |
| LIST | 100 obj | 50 | 34ms | 32ms | 40ms | - |
| DELETE | 1KB | 100 | 4.3ms | 4.3ms | 5.2ms | - |

### 4 disks (EC 3+1)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 12.1ms | 10.3ms | 32.6ms | 83 KB/s |
| PUT | 1MB | 20 | 93ms | 90ms | 119ms | 10.7 MB/s |
| PUT | 10MB | 5 | 767ms | 735ms | 764ms | 13.0 MB/s |
| GET | 1KB | 100 | 3.6ms | 3.3ms | 11.7ms | 279 KB/s |
| GET | 1MB | 20 | 12ms | 12ms | 14ms | 81 MB/s |
| GET | 10MB | 5 | 91ms | 89ms | 94ms | 110 MB/s |
| HEAD | - | 100 | 2.3ms | 2.3ms | 3.2ms | - |
| LIST | 100 obj | 50 | 37ms | 36ms | 49ms | - |
| DELETE | 1KB | 100 | 5.1ms | 5.0ms | 6.6ms | - |

## Observations

**GET throughput is excellent.** 127 MB/s on 1 disk, 110 MB/s on 4 disks. The
bottleneck is disk I/O, not CPU or protocol overhead.

**PUT throughput is disk-bound.** ~10-13 MB/s across all disk counts. All volumes
are on the same physical NTFS drive, so parallel shard writes contend for the
same I/O. With separate physical disks, PUT throughput should scale linearly.

**PUT latency scales with disk count for small objects.** 5.7ms (1 disk) to 10.3ms
(4 disks) p50 for 1KB. More shards = more metadata writes.

**HEAD is consistently fast.** ~2.3ms p50 regardless of disk count.

## Improvement history

| Metric | v1 (debug, sequential, std::fs) | v3 (release, parallel, tokio::fs) | Speedup |
|---|---|---|---|
| GET 10MB, 1 disk | 16.7 MB/s | 127 MB/s | **7.6x** |
| GET 10MB, 4 disks | 11.4 MB/s | 110 MB/s | **9.6x** |
| PUT 10MB, 4 disks | 4.4 MB/s | 13.0 MB/s | **3.0x** |
| PUT 1MB, 4 disks | 4.1 MB/s | 10.7 MB/s | **2.6x** |

Contributors: release-mode optimized crypto (SHA256, MD5, Reed-Solomon), parallel
shard I/O via join_all, non-blocking disk I/O via tokio::fs.

## Remaining bottlenecks

- **Single physical disk**: all test volumes share one NTFS drive. Separate disks
  would show real parallelism benefits.
- **SHA256 for shard checksums**: could switch to blake3 for 2-3x faster hashing.
- **reed-solomon-erasure**: no SIMD. `reed-solomon-simd` would be 2-5x faster.
- **Full body buffering**: entire object loaded into Vec<u8>. Streaming would
  reduce memory and enable pipelining.

## Running benchmarks

Release binary (recommended):
```
cd abixio && cargo build --release
cd abixio-ui
ABIXIO_BIN='C:\code\endless\rust\target\release\abixio.exe' cargo test --test bench -- --ignored --nocapture
```

Single config:
```
cargo test --test bench -- --ignored --nocapture bench_4_disks
```
