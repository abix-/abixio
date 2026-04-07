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

### Raw disk baseline (tokio::fs, no abixio)

Single file write/read to tmpdir via `tokio::fs`. This is the theoretical
maximum -- OS page cache, no erasure coding, no hashing, no HTTP, no metadata.

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| WRITE | 1KB | 200 | 372us | 325us | 860us | 2.6 MB/s |
| WRITE | 1MB | 50 | 1.7ms | 1.6ms | 3.6ms | 593 MB/s |
| WRITE | 10MB | 10 | 6.8ms | 6.1ms | 6.7ms | 1,474 MB/s |
| READ | 1KB | 200 | 80us | 78us | 98us | 12.3 MB/s |
| READ | 1MB | 50 | 492us | 471us | 630us | 2,032 MB/s |
| READ | 10MB | 10 | 4.3ms | 4.3ms | 4.7ms | 2,330 MB/s |

Note: high throughput is OS page cache (tmpdir fits in RAM). These numbers
represent the ceiling, not real disk throughput.

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

**Overhead vs raw disk.** The raw baseline shows what the OS can do without
abixio in the way. The gap is the cost of being a storage server.

| Operation | Raw disk | AbixIO 1 disk | Overhead | Where it goes |
|---|---|---|---|---|
| WRITE 10MB | 1,474 MB/s | 12.4 MB/s | 119x | EC encode, SHA256, meta.json, HTTP |
| READ 10MB | 2,330 MB/s | 127 MB/s | 18x | SHA256 verify, HTTP, body buffering |
| WRITE 1MB | 593 MB/s | 9.4 MB/s | 63x | same |
| READ 1MB | 2,032 MB/s | 101 MB/s | 20x | same |
| WRITE 1KB | 2.6 MB/s | 152 KB/s | 17x | per-request fixed cost dominates |
| READ 1KB | 12.3 MB/s | 293 KB/s | 42x | per-request fixed cost dominates |

Raw numbers are inflated by OS page cache (tmpdir fits in RAM). The ratios show
where the real overhead lives.

**GET is 6x closer to raw than PUT.** GET skips erasure encoding and only reads
data shards (not parity). The remaining gap is SHA256 verification, HTTP
serialization, and full-body buffering.

**PUT overhead is dominated by per-shard work.** Each shard requires: SHA256 hash,
file create (shard.dat), file create (meta.json), directory ensure. With 4 disks
that's 12 filesystem operations per PUT.

**Small object latency is HTTP-bound.** 1KB PUT at 5.7ms vs raw WRITE at 0.3ms.
The 5ms gap is HTTP round-trip, SigV4 verification, s3s routing, and response
serialization. The data itself is negligible.

**PUT throughput is flat across disk counts.** ~10-13 MB/s regardless of 1-4 disks.
All volumes share one physical drive, so parallel shard writes contend for the
same I/O. With separate physical disks, PUT should scale linearly.

**HEAD is consistently fast.** ~2.3ms p50 regardless of disk count. Reads one
metadata file, no shard data.

## Improvement history

| Metric | v1 (debug, seq, std::fs) | v3 (tokio::fs, SHA256) | v4 (blake3) | Total speedup |
|---|---|---|---|---|
| GET 10MB, 1 disk | 16.7 MB/s | 127 MB/s | **228 MB/s** | **13.7x** |
| GET 10MB, 4 disks | 11.4 MB/s | 110 MB/s | **208 MB/s** | **18.2x** |
| GET 1MB, 4 disks | 4.1 MB/s | 81 MB/s | **162 MB/s** | **39.5x** |
| PUT 10MB, 4 disks | 4.4 MB/s | 13.0 MB/s | **13.3 MB/s** | **3.0x** |

Contributors:
- v3: release build, parallel shard I/O (join_all), tokio::fs
- v4: blake3 shard checksums (15x faster than SHA256, still cryptographic)

### Storage layer timing (direct VolumePool, 1 disk, 10MB)

| Step | Time | Notes |
|---|---|---|
| MD5 (ETag) | 14ms | Spec-required, cannot remove |
| blake3 (shard checksum) | 2.4ms | Was 37ms with SHA256 |
| Reed-Solomon encode | ~5ms | No SIMD yet |
| tokio::fs::write | 6ms | OS page cache |
| **Total storage** | **43ms** | Was 60ms before blake3 |
| **HTTP overhead** | **~650ms** | Body buffering via collect_body() |
| **Total through HTTP** | **~700ms** | Storage is only 6% of total time |

## Remaining bottlenecks

- **HTTP body buffering (93% of PUT time)**: `collect_body()` reads the entire
  request body into `Vec<u8>` before processing. A streaming pipeline would
  eliminate this 650ms penalty on 10MB objects.
- **MD5 for ETag (33% of storage time)**: spec-required, cannot remove. Could
  compute inline during body read (single-pass) instead of a separate full pass.
- **reed-solomon-erasure**: no SIMD. `reed-solomon-simd` would be 2-5x faster.
- **Single physical disk**: all test volumes share one NTFS drive.

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
