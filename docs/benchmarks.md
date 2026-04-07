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

Single `tokio::fs` write/read to tmpdir. No abixio, no HTTP, no encoding.
This is the ceiling -- OS page cache, not real disk speed.

| Operation | 1KB | 1MB | 10MB |
|---|---|---|---|
| WRITE | 2.6 MB/s (372us) | 593 MB/s (1.7ms) | 1,474 MB/s (6.8ms) |
| READ | 12.3 MB/s (80us) | 2,032 MB/s (492us) | 2,330 MB/s (4.3ms) |

AbixIO PUT 10MB at 147 MB/s vs raw write at 1,474 MB/s = 10x overhead from
erasure coding, checksums, metadata, and HTTP protocol. GET at 249 MB/s vs
raw read at 2,330 MB/s = 9x overhead.

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
