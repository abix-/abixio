# Benchmarks

## vs RustFS vs MinIO

All three servers running on Windows 10, same NTFS drive, single disk, no
erasure coding. `mc` client with SigV4 auth, sequential requests, 15 iterations.
All connections via `127.0.0.1` (never `localhost` -- Windows DNS resolution
of `localhost` adds 200ms per request).

### PUT

![PUT throughput](img/bench-put.svg)

At 10MB, all three are within noise (81-92 MB/s). At 1GB, AbixIO (440)
beats MinIO (376) and trails RustFS (540).

### GET

![GET throughput](img/bench-get.svg)

At 10MB, all three are close (106-122 MB/s). At 1GB through `mc`, MinIO
(953) leads. AbixIO (386) is bottlenecked by the `mc` client, not the
server -- direct curl shows **1220 MB/s** for a 1GB GET.

### What the mc client hides

| Measurement | AbixIO 1GB GET |
|-------------|---------------|
| Through `mc` (SigV4, write to disk) | 386 MB/s |
| Through curl (no auth, /dev/null) | **1220 MB/s** |
| Internal L6 benchmark (no client) | **568 MB/s** |
| Internal L4 storage layer | **page cache speed** |

MinIO gets 953 MB/s through the same `mc` because `mc` is a Go binary
optimized for Go's HTTP stack. This is a client-side gap, not server-side.

---

## 4KB small object performance

The log-structured storage path eliminates filesystem metadata overhead
for small objects. Measured via curl with `127.0.0.1`, Content-Length header.

### Latency breakdown (4KB PUT)

```
File tier:                          Log store:
  TCP connect:     0.68ms             TCP connect:     0.68ms
  server process:  1.33ms             server process:  0.53ms  <- 60% faster
  total:           2.01ms             total:           1.21ms
```

### Latency breakdown (4KB GET)

```
File tier:                          Log store:
  TCP connect:     0.69ms             TCP connect:     0.69ms
  server process:  0.95ms             server process:  0.31ms  <- 67% faster
  total:           1.69ms             total:           1.00ms
```

### Summary

| | File tier | Log store | Improvement |
|--|----------|-----------|-------------|
| **4KB PUT total** | 2.0ms | **1.2ms** | **40% faster** |
| **4KB GET total** | 1.7ms | **1.0ms** | **41% faster** |
| **4KB PUT server only** | 1.33ms | **0.53ms** | **60% faster** |
| **4KB GET server only** | 0.95ms | **0.31ms** | **67% faster** |
| Filesystem ops (4 disks) | 12 | **4** | 3x fewer |
| Files per 1M objects | 3M+ | ~3 segments | ~1000x fewer |

TCP connect (0.68ms) is 57% of log store total -- fixed cost from Windows
TCP stack. Linux localhost connect is ~0.03ms (20x faster). With HTTP
keep-alive (persistent connections), TCP connect happens once per session.

### Windows localhost warning

**Never use `localhost` for benchmarks on Windows.** DNS resolution of
`localhost` on Windows adds ~200ms per request (IPv6 fallback). Always
use `127.0.0.1`.

```
curl http://localhost:10000/...    <- 210ms (DNS: 200ms + actual: 10ms)
curl http://127.0.0.1:10000/...   <- 1.2ms (actual performance)
```

---

## How fast is each layer?

### GET performance (1 disk, 10MB / 1GB)

| Layer | What | 10MB | 1GB | Bottleneck? |
|-------|------|------|-----|-------------|
| L6 | Full S3 GET | 253 MB/s | 568 MB/s | s3s overhead |
| L5 | Raw HTTP (no S3) | 762 MB/s | 800 MB/s | no |
| L4 | Storage + mmap | 19,031 MB/s | page cache | no |
| L3 | Disk read (cached) | 2703 MB/s | 2902 MB/s | no |

For 1+0 (no EC), GET is zero-copy: mmap the shard file, yield the entire
mapping as a single `Bytes` through hyper. Direct curl: 1220 MB/s at 1GB.

### GET performance (4 disk, 3+1 EC, 10MB / 1GB)

| Layer | What | 10MB | 1GB |
|-------|------|------|-----|
| L4 | Storage + mmap EC (zero-copy) | 10,937 MB/s | page cache |
| L4 | Storage (old buffered path) | 774 MB/s | 919 MB/s |

EC GET is zero-copy: mmap shard slices yielded directly as `Bytes` frames.

### PUT performance (1 disk, 10MB)

![PUT breakdown](img/bench-breakdown.svg)

| Layer | What | Speed | Bottleneck? |
|-------|------|-------|-------------|
| L6 | Full S3 PUT | 257 MB/s | s3s overhead |
| L5 | Raw HTTP | 762 MB/s | no |
| L4 | Storage pipeline | 483 MB/s | hashing + encode |
| L3 | Disk write (page cache) | 1625 MB/s | no |
| L2 | RS encode (SIMD) | 2762 MB/s | no |
| L1 | MD5 hash (S3 ETag) | 703 MB/s | floor |

PUT is zero-allocation in the per-block encode loop.

---

## Reproducing

### Competitive benchmark (vs RustFS, MinIO)

```bash
cargo build --release
ABIXIO_BIN=./target/release/abixio RUSTFS_BIN=rustfs MINIO_BIN=minio MC=mc \
    ITERS=15 SIZES="4096 10485760 1073741824" \
    bash tests/compare_bench.sh
```

### Per-layer benchmark

```bash
cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# compare after a change
BENCH_COMPARE=bench-results/baseline.json \
    cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# specific layers or sizes
BENCH_LAYERS=L4,L6 BENCH_SIZES=10485760 \
    cargo test --release --test layer_bench -- --ignored bench_perf --nocapture
```

---

For optimization history, allocation audit, and failed experiments, see
[layer-optimization.md](layer-optimization.md).

For log-structured storage design, see [write-log.md](write-log.md).
