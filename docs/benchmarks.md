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
- blake3 shard checksums, streaming block-based encode

## Results (2026-04-07, release build)

### Layer isolation (10MB, 1 disk)

Where does the time actually go? Each row measures one layer in isolation.

| Layer | What it measures | Time | Throughput |
|---|---|---|---|
| tokio::fs::write | Raw disk write (OS cache) | 7ms | 1,285 MB/s |
| blake3 hash | Shard integrity checksum | 3ms | 3,400 MB/s |
| MD5 hash | ETag computation | 15ms | 630 MB/s |
| RS encode 3+1 | Reed-Solomon erasure coding | 4ms | 2,509 MB/s |
| **VolumePool direct** | Full storage layer (no HTTP) | **29ms** | **350 MB/s** |
| reqwest -> hyper | Raw HTTP body transfer | 47ms | 238 MB/s |
| reqwest -> s3s -> storage | HTTP + s3s protocol + storage | 41ms | 132 MB/s |
| **aws-sdk-s3 (unsigned)** | **S3 client, UNSIGNED-PAYLOAD** | **68ms** | **147 MB/s** |
| aws-sdk-s3 (signed) | S3 client, full SigV4 body hash | 700ms | 14 MB/s |

**Key finding:** the signed aws-sdk-s3 client computes SHA256 of the entire body
before sending (`x-amz-content-sha256`). On 10MB that adds ~630ms of pure
client-side hashing. The server processes the request in ~40ms.

With `UNSIGNED-PAYLOAD` (skips client body SHA256), PUT 10MB runs at **147 MB/s**.

### 1 disk (EC 1+0, no parity)

| Operation | Size | Ops | Avg | p50 | Throughput |
|---|---|---|---|---|---|
| PUT (signed) | 10MB | 5 | 705ms | 646ms | 14 MB/s |
| **PUT (unsigned)** | **10MB** | **5** | **68ms** | **68ms** | **147 MB/s** |
| PUT | 1MB | 20 | 68ms | 68ms | 14.7 MB/s |
| PUT | 1KB | 100 | 4.0ms | 3.9ms | 252 KB/s |
| GET | 10MB | 5 | 51ms | 44ms | 195 MB/s |
| GET | 1MB | 20 | 5.6ms | 5.6ms | 179 MB/s |
| GET | 1KB | 100 | 2.9ms | 2.9ms | 344 KB/s |
| HEAD | - | 100 | 2.4ms | 2.3ms | - |
| LIST | 100 obj | 50 | 25.6ms | 24.9ms | - |
| DELETE | 1KB | 100 | 2.9ms | 2.9ms | - |

### 4 disks (EC 3+1)

| Operation | Size | Ops | Avg | p50 | Throughput |
|---|---|---|---|---|---|
| PUT (signed) | 10MB | 5 | 701ms | 690ms | 14 MB/s |
| **PUT (unsigned)** | **10MB** | **5** | **~75ms** | **~73ms** | **~133 MB/s** |
| GET | 10MB | 5 | 40ms | 39ms | 249 MB/s |
| GET | 1MB | 20 | 7.1ms | 6.7ms | 140 MB/s |
| HEAD | - | 100 | 2.3ms | 2.2ms | - |

## Improvement history

| Metric | v1 (start) | v5 (current, unsigned) | Total speedup |
|---|---|---|---|
| PUT 10MB, 1 disk | 5.5 MB/s | **147 MB/s** | **26.7x** |
| GET 10MB, 1 disk | 16.7 MB/s | **195 MB/s** | **11.7x** |
| GET 10MB, 4 disks | 11.4 MB/s | **249 MB/s** | **21.8x** |

Changes applied:
- v2: release build, parallel shard I/O (join_all)
- v3: tokio::fs (non-blocking disk I/O)
- v4: blake3 shard checksums (15x faster than SHA256)
- v5: streaming encode pipeline, inline MD5, UNSIGNED-PAYLOAD client option

## How the signed vs unsigned tradeoff works

Standard S3 clients (aws-sdk, boto3) compute SHA256 of the entire request body
before sending, per the SigV4 spec. This ensures the server can verify body
integrity. For a 10MB upload, SHA256 takes ~40ms on the client.

`UNSIGNED-PAYLOAD` tells the server to skip body hash verification. The SigV4
signature still covers the headers (authentication is intact), but the body
content is not signed. This is safe for:
- Trusted networks (internal, VPN, localhost)
- HTTPS connections (TLS provides integrity)
- When the server computes its own checksums (we do: blake3 per shard)

RustFS and MinIO both default to UNSIGNED-PAYLOAD for HTTP connections. The
aws-sdk-s3 Rust crate supports it via `.customize().disable_payload_signing()`.

## Running benchmarks

Layer isolation (direct, no HTTP):
```
cd abixio
cargo test --release --test layer_bench -- --ignored --nocapture
```

Full HTTP (via aws-sdk-s3):
```
cd abixio && cargo build --release
cd abixio-ui
ABIXIO_BIN='path/to/release/abixio.exe' cargo test --test bench -- --ignored --nocapture
```
