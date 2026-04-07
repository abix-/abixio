# Benchmarks

Real S3 operations through the full stack: aws-sdk-s3 -> HTTP -> hyper -> s3s ->
VolumePool -> erasure encode -> LocalVolume -> tokio::fs -> disk.

## Test setup

- Windows 10, debug build (not release-optimized)
- All volumes on same NTFS drive (tmpdir)
- Single-node, local disk only
- aws-sdk-s3 client with SigV4 authentication
- No concurrent requests (sequential, single client)

## Results (2026-04-07, debug build)

### 1 disk (EC 1+0, no parity)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 4.8ms | 4.6ms | 7.5ms | 209 KB/s |
| PUT | 1MB | 20 | 180ms | 178ms | 196ms | 5.6 MB/s |
| PUT | 10MB | 5 | 1.82s | 1.74s | 1.87s | 5.5 MB/s |
| GET | 1KB | 100 | 3.9ms | 3.8ms | 5.3ms | 254 KB/s |
| GET | 1MB | 20 | 60ms | 60ms | 63ms | 16.7 MB/s |
| GET | 10MB | 5 | 599ms | 593ms | 603ms | 16.7 MB/s |
| HEAD | - | 100 | 3.2ms | 2.9ms | 4.8ms | - |
| LIST | 100 obj | 50 | 25.8ms | 25.3ms | 31.3ms | - |
| DELETE | 1KB | 100 | 3.3ms | 3.2ms | 4.5ms | - |

### 2 disks (EC 1+1)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 8.7ms | 6.9ms | 35.8ms | 114 KB/s |
| PUT | 1MB | 20 | 289ms | 277ms | 375ms | 3.5 MB/s |
| PUT | 10MB | 5 | 2.71s | 2.66s | 2.68s | 3.7 MB/s |
| GET | 1KB | 100 | 3.8ms | 3.7ms | 5.2ms | 267 KB/s |
| GET | 1MB | 20 | 121ms | 117ms | 137ms | 8.3 MB/s |
| GET | 10MB | 5 | 1.22s | 1.14s | 1.20s | 8.2 MB/s |
| HEAD | - | 100 | 3.6ms | 3.0ms | 18.7ms | - |
| LIST | 100 obj | 50 | 28.2ms | 27.8ms | 30.3ms | - |
| DELETE | 1KB | 100 | 4.4ms | 3.8ms | 4.6ms | - |

### 3 disks (EC 2+1)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 11.4ms | 8.2ms | 63.8ms | 88 KB/s |
| PUT | 1MB | 20 | 247ms | 243ms | 270ms | 4.0 MB/s |
| PUT | 10MB | 5 | 2.38s | 2.35s | 2.41s | 4.2 MB/s |
| GET | 1KB | 100 | 4.2ms | 4.1ms | 5.1ms | 238 KB/s |
| GET | 1MB | 20 | 92ms | 88ms | 112ms | 10.9 MB/s |
| GET | 10MB | 5 | 946ms | 907ms | 978ms | 10.6 MB/s |
| HEAD | - | 100 | 2.9ms | 2.8ms | 3.7ms | - |
| LIST | 100 obj | 50 | 34.1ms | 33.6ms | 39.6ms | - |
| DELETE | 1KB | 100 | 4.6ms | 4.5ms | 7.9ms | - |

### 4 disks (EC 3+1)

| Operation | Size | Ops | Avg | p50 | p99 | Throughput |
|---|---|---|---|---|---|---|
| PUT | 1KB | 100 | 12.9ms | 10.4ms | 32.8ms | 77 KB/s |
| PUT | 1MB | 20 | 242ms | 234ms | 272ms | 4.1 MB/s |
| PUT | 10MB | 5 | 2.29s | 2.25s | 2.29s | 4.4 MB/s |
| GET | 1KB | 100 | 4.3ms | 4.2ms | 5.2ms | 231 KB/s |
| GET | 1MB | 20 | 82ms | 80ms | 97ms | 12.2 MB/s |
| GET | 10MB | 5 | 875ms | 825ms | 854ms | 11.4 MB/s |
| HEAD | - | 100 | 3.2ms | 3.0ms | 4.0ms | - |
| LIST | 100 obj | 50 | 36.2ms | 36.0ms | 39.3ms | - |
| DELETE | 1KB | 100 | 5.1ms | 5.0ms | 6.1ms | - |

## Observations

**PUT scales with disk count.** 1-disk PUT 1KB at 4.8ms, 4-disk at 12.9ms. This
is expected: more disks = more erasure shards = more I/O per write. The overhead
is roughly linear.

**GET is faster than PUT.** Reed-Solomon decode reads `data_n` shards (not all
shards), so GET avoids reading parity. GET 10MB at 16.7 MB/s (1 disk) vs PUT
10MB at 5.5 MB/s.

**HEAD is consistently fast.** ~3ms regardless of disk count. Only reads metadata,
no shard data.

**LIST scales mildly with disk count.** 25ms (1 disk) to 36ms (4 disks). LIST
reads metadata from all disks and deduplicates.

**Debug build penalty.** These numbers are from `cargo test` (debug profile). A
release build would be significantly faster due to optimized Reed-Solomon, SHA256,
and serde.

**All volumes on same physical disk.** Real deployments with separate disks would
see better parallelism for multi-disk writes.

## Running benchmarks

```
cd abixio-ui
cargo test --test bench -- --ignored --nocapture
```

Single config:
```
cargo test --test bench -- --ignored --nocapture bench_4_disks
```
