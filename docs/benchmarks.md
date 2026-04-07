# Benchmarks

## Setup

- Windows 10, single machine, all volumes on same NTFS drive (tmpdir)
- Single-node, 1 disk, no erasure coding (1+0)
- Sequential requests, no concurrency, page-cache writes (no fsync)
- 10 iterations per measurement (50 for 4KB, 5 for 1GB)
- `mc` client with UNSIGNED-PAYLOAD over HTTP
- AbixIO `037106f`, RustFS 1.0.0-alpha.90, MinIO RELEASE.2026-04-07

## PUT throughput

![PUT throughput](img/bench-put.svg)

At 4KB, all three servers are bottlenecked by `mc` process startup (~70ms).
At 10MB, throughput is comparable across servers (74-90 MB/s).
At 1GB, AbixIO (375 MB/s) trails RustFS (541) and MinIO (576). The storage
layer does 478 MB/s at 1GB (see below), so the gap is HTTP/s3s overhead.

## GET throughput

![GET throughput](img/bench-get.svg)

MinIO GET scales better at 1GB (801 MB/s) than both Rust servers.
At 10MB, all three are in the same range (96-122 MB/s).

## Where the time goes (10MB PUT)

![PUT breakdown](img/bench-breakdown.svg)

MD5 and blake3 are computed inline during the streaming read -- their cost
overlaps with network I/O for large objects. Client SigV4 is not server-side.

## Per-layer breakdown

Each layer of the PUT path benchmarked independently at 4KB, 10MB, and 1GB.
This is how we validate optimizations -- change one layer, measure its impact
without touching the rest of the stack.

```
Layer  What it measures                              10MB         1GB
-----  -------------------------------------------  -----------  -----------
L1     blake3 hash (shard integrity)                 4418 MB/s    4247 MB/s
L1     MD5 hash (S3 ETag)                             702 MB/s     688 MB/s
L2     RS encode 3+1 (reed-solomon, SIMD)            2627 MB/s    2744 MB/s
L3     tokio::fs::write (disk ceiling)               1373 MB/s    1018 MB/s
L3     tokio::fs::read                               2236 MB/s    2762 MB/s
L4     VolumePool put_stream (1 disk, integrated)     424 MB/s     478 MB/s
L4     VolumePool get (1 disk, integrated)           1191 MB/s    1224 MB/s
L4     VolumePool put_stream (4 disk, 3+1 EC)         366 MB/s     402 MB/s
L4     VolumePool get (4 disk, 3+1 EC)                838 MB/s     921 MB/s
```

**What each layer tells you:**

- **L1** -- MD5 is 6.3x slower than blake3. If we could drop MD5 from the hot
  path, hashing overhead nearly disappears. (MD5 is computed inline during
  the stream, so it overlaps with I/O for large objects.)
- **L2** -- RS encode at 2627 MB/s is not a bottleneck. SIMD-accel is enabled.
- **L3** -- Disk write at 1373 MB/s is the ceiling. L4 at 424 MB/s = 3.2x
  overhead from hashing + RS + metadata writes.
- **L4** -- The integrated storage path. This is where ShardWriter trait
  dispatch, buffer management, and parallel writes show up. 4-disk EC adds
  ~15% overhead vs 1-disk at 10MB (extra RS encode + 4x shard I/O on same drive).

## Reproducing these benchmarks

### Competitive benchmark (AbixIO vs RustFS vs MinIO)

Requires all three server binaries and `mc` (MinIO client).

```bash
# build abixio
cargo build --release

# download competitors (windows example)
curl -fSL -o rustfs.exe https://github.com/rustfs/rustfs/releases/download/1.0.0-alpha.90/rustfs-windows-x86_64-v1.0.0-alpha.90.zip
curl -fSL -o minio.exe https://dl.min.io/server/minio/release/windows-amd64/minio.exe
curl -fSL -o mc.exe https://dl.min.io/client/mc/release/windows-amd64/mc.exe

# run (any binary can be omitted -- that column shows "skip")
ABIXIO_BIN=./target/release/abixio RUSTFS_BIN=./rustfs.exe MINIO_BIN=./minio.exe MC=./mc.exe \
    ITERS=10 SIZES="4096 10485760 1073741824" \
    bash tests/compare_bench.sh
```

### Per-layer benchmark (storage internals)

No external binaries needed. Saves JSON to `bench-results/` for A/B comparison.

```bash
# run all layers at all sizes (~3 minutes)
cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# compare against a baseline after making a change
BENCH_COMPARE=bench-results/baseline.json \
    cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# run only specific layers (fast iteration)
BENCH_LAYERS=L1,L2 \
    cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# run only specific sizes
BENCH_SIZES=10485760 \
    cargo test --release --test layer_bench -- --ignored bench_perf --nocapture
```

The comparison output flags regressions (>5% slower) and improvements (>5% faster)
per layer, per size, so you can see exactly what a change affected.
