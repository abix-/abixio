# Benchmarks

S3 operations through the full stack: S3 client -> HTTP -> hyper -> s3s ->
VolumePool -> erasure encode -> LocalVolume -> tokio::fs -> disk.

## Setup

- Windows 10, single machine, all volumes on same NTFS drive (tmpdir)
- Single-node, local disk only, sequential requests (no concurrency)
- blake3 shard checksums, streaming block-based RS encode, parallel shard I/O

## PUT throughput

| Disks | EC | 1KB | 1MB | 10MB |
|---|---|---|---|---|
| 1 | 1+0 | 252 KB/s (4.0ms) | 14.7 MB/s (68ms) | **147 MB/s** (68ms) |
| 4 | 3+1 | 80 KB/s (12.5ms) | 11.2 MB/s (90ms) | **133 MB/s** (~75ms) |

## GET throughput

| Disks | EC | 1KB | 1MB | 10MB |
|---|---|---|---|---|
| 1 | 1+0 | 344 KB/s (2.9ms) | 179 MB/s (5.6ms) | **195 MB/s** (51ms) |
| 4 | 3+1 | 273 KB/s (3.7ms) | 140 MB/s (7.1ms) | **249 MB/s** (40ms) |

## Other operations

| Operation | 1 disk | 4 disks |
|---|---|---|
| HEAD | 2.3ms | 2.2ms |
| LIST (100 objects) | 25ms | 37ms |
| DELETE | 2.9ms | 4.8ms |

## Raw disk baseline

Single file write/read via `tokio::fs`. No abixio, no HTTP, no encoding.

| Operation | 1KB | 1MB | 10MB |
|---|---|---|---|
| WRITE (cached) | 2.7 MB/s | 785 MB/s | 1,287 MB/s |
| **WRITE + fsync (real disk)** | - | - | **879 MB/s (11ms)** |
| READ (cached) | 11.8 MB/s | 2,169 MB/s | 2,574 MB/s |

The cached numbers hit OS page cache (data stays in RAM). The fsync number
forces data to the physical disk -- this is the honest baseline.

AbixIO also writes without fsync (like most object stores), so the fair
comparison is against the cached write:

- PUT 10MB at 147 MB/s vs cached write at 1,287 MB/s = **8.8x overhead**
- GET 10MB at 249 MB/s vs cached read at 2,574 MB/s = **10.3x overhead**

The overhead comes from: erasure coding (4ms), blake3 checksums (3ms),
MD5 ETag (15ms), metadata writes, HTTP protocol, and s3s SigV4 processing.

## Where the time goes

Layer isolation benchmark (`tests/layer_bench.rs`), 10MB, 1 disk:

| Layer | Time | Throughput | Notes |
|---|---|---|---|
| tokio::fs::write | 7ms | 1,285 MB/s | OS page cache |
| blake3 hash | 3ms | 3,400 MB/s | Shard integrity |
| MD5 hash | 15ms | 630 MB/s | ETag (S3 spec) |
| RS encode 3+1 | 4ms | 2,509 MB/s | Erasure coding |
| **VolumePool direct** | **29ms** | **350 MB/s** | Storage layer total |
| HTTP + s3s + storage | 41ms | 132 MB/s | No SigV4 |
| **aws-sdk-s3 unsigned** | **68ms** | **147 MB/s** | Real-world performance |

Most S3 clients (`mc`, `rclone`, `aws cli`) use UNSIGNED-PAYLOAD or chunked
signatures over HTTP, avoiding the upfront body SHA256. The aws-sdk-s3 Rust
crate defaults to full body hashing but supports `.customize().disable_payload_signing()`
for the standard behavior.

## Client compatibility

| Client | Default for HTTP | Body SHA256? | Expected throughput |
|---|---|---|---|
| MinIO `mc` | UNSIGNED-PAYLOAD | No | ~147 MB/s |
| `rclone` | UNSIGNED-PAYLOAD | No | ~147 MB/s |
| `aws cli` | Chunked streaming | Streamed, not upfront | ~130 MB/s |
| aws-sdk-s3 (Rust) | Full body hash | Yes (40ms/10MB) | 14 MB/s |
| aws-sdk-s3 + unsigned | UNSIGNED-PAYLOAD | No | ~147 MB/s |
| `boto3` / `aws-sdk` (other) | Varies by config | Configurable | ~130-147 MB/s |

## vs RustFS vs MinIO

3-way head-to-head on the same NTFS volume. All servers run single-node,
single-disk, no EC, tmpdir storage. `mc` client with UNSIGNED-PAYLOAD over HTTP.

Setup: Windows 10, same NTFS drive, sequential requests, 10 iterations.
RustFS 1.0.0-alpha.90, MinIO RELEASE.2026-04-07 (archived).

### 10MB throughput (10 iterations)

| Operation | AbixIO | RustFS | MinIO |
|---|---|---|---|
| PUT | **147 MB/s** (68ms) | 77 MB/s (130ms) | 88 MB/s (114ms) |
| GET | **143 MB/s** (70ms) | 99 MB/s (101ms) | 124 MB/s (81ms) |

### 1GB throughput (10 iterations)

| Operation | AbixIO | RustFS | MinIO |
|---|---|---|---|
| PUT | 353 MB/s (2.9s) | 464 MB/s (2.2s) | **537 MB/s** (1.9s) |
| GET | 372 MB/s (2.75s) | 576 MB/s (1.78s) | **849 MB/s** (1.2s) |

### Metadata operations (10 iterations)

| Operation | AbixIO | RustFS | MinIO |
|---|---|---|---|
| HEAD | 65ms | 64ms | 64ms |
| LIST (100 objects) | 63ms | 63ms | 63ms |
| DELETE | 64ms | 66ms | 66ms |

### Storage layer throughput (no HTTP, VolumePool direct)

Single-node, tmpdir, 10 iterations, page-cache writes.

| Op | Disks | 1MB | 10MB | 100MB | 1GB |
|---|---|---|---|---|---|
| put_stream | 1 | 248 MB/s | 436 MB/s | 480 MB/s | 468 MB/s |
| get | 1 | 715 MB/s | 1164 MB/s | 1143 MB/s | 790 MB/s |
| put_stream | 4 | 197 MB/s | 377 MB/s | 435 MB/s | 382 MB/s |
| get | 4 | 90 MB/s | 736 MB/s | 933 MB/s | 564 MB/s |

The storage layer does 468 MB/s PUT at 1GB -- HTTP/s3s/mc overhead accounts
for the gap between these numbers and the mc-based competitive benchmark.

### Notes

- AbixIO dominates at 10MB via mc: **1.9x faster PUT than RustFS, 1.7x faster than MinIO**
- At 1GB via mc, AbixIO (353 MB/s) trails RustFS (464) and MinIO (537) due to HTTP overhead
- Storage layer at 1GB (468 MB/s) is competitive with RustFS's mc throughput
- Metadata operations (HEAD, LIST, DELETE) are within noise across all three
- All numbers are page-cache writes (no fsync). Real disk throughput is lower
- mc benchmark verifies round-trip: PUT then GET, size check before timing
- Internal bench (`bench_perf`) saves JSON to bench-results/ for A/B comparison

## Running benchmarks

Layer isolation (no HTTP overhead):
```
cd abixio
cargo test --release --test layer_bench -- --ignored --nocapture
```

Full S3 client benchmark:
```
cd abixio && cargo build --release
cd abixio-ui
ABIXIO_BIN='path/to/release/abixio.exe' cargo test --test bench -- --ignored --nocapture
```

Comparative benchmark (AbixIO vs RustFS vs MinIO):
```
cd abixio && cargo build --release
ABIXIO_BIN=./target/release/abixio RUSTFS_BIN=rustfs MINIO_BIN=minio bash tests/compare_bench.sh
```

Any server binary can be omitted -- that column shows "skip".

Tuning: `ITERS=10` for more stable numbers, `SIZES="10485760"` to test one size.
