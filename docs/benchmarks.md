# Benchmarks

## Setup

- Windows 10, single machine, all volumes on same NTFS drive (tmpdir)
- Single-node, 1 disk, no erasure coding (1+0) unless noted
- Sequential requests, no concurrency, page-cache writes (no fsync)
- 10 iterations per measurement (50 for 4KB, 5 for 1GB)
- Per-layer numbers from `bench_perf` at commit `911e08d`
- Competitive numbers from `mc` client with UNSIGNED-PAYLOAD over HTTP
  against AbixIO `911e08d`, RustFS 1.0.0-alpha.90, MinIO RELEASE.2026-04-07

For per-layer optimization details, methodology, and failed experiments,
see [layer-optimization.md](layer-optimization.md).

## PUT throughput

![PUT throughput](img/bench-put.svg)

At 4KB, all three servers are bottlenecked by `mc` process startup (~67ms).
At 10MB, throughput is comparable across servers (78-91 MB/s).
At 1GB, AbixIO (426 MB/s) trails RustFS (520) and MinIO (543). The storage
layer does 497 MB/s at 1GB (see below), so the gap is mostly s3s overhead.

## GET throughput

![GET throughput](img/bench-get.svg)

At 10MB, all three servers are in the same range (116-131 MB/s).
At 1GB through `mc`, MinIO (1057 MB/s) leads; AbixIO (366) is bottlenecked
by `mc` client overhead, not the server.

Direct measurement (curl, no auth, no disk write) shows AbixIO serving 1GB
at **1220 MB/s** -- faster than MinIO through `mc`. The 1+0 mmap fast path
serves files directly from page cache without copying or RS decode.

## Where the time goes (10MB PUT)

![PUT breakdown](img/bench-breakdown.svg)

MD5 and blake3 are computed inline during the streaming read -- their cost
overlaps with network I/O for large objects. Client SigV4 is not server-side.

## Per-layer breakdown

Each layer of the PUT path benchmarked independently at 4KB, 10MB, and 1GB.
This is how we validate optimizations -- change one layer, measure its impact
without touching the rest of the stack.

```
Layer  What it measures                              4KB          10MB         1GB
-----  -------------------------------------------  -----------  -----------  -----------
L1     blake3 hash (shard integrity)                 1604 MB/s    4303 MB/s    4286 MB/s
L1     MD5 hash (S3 ETag)                             449 MB/s     703 MB/s     700 MB/s
L2     RS encode 3+1 (reed-solomon, SIMD)            2955 MB/s    2762 MB/s    2825 MB/s
L3     tokio::fs::write (disk ceiling)                 13 MB/s    1625 MB/s    1056 MB/s
L3     tokio::fs::read                                 46 MB/s    2703 MB/s    2902 MB/s
L4     VolumePool put_stream (1 disk)                   4 MB/s     439 MB/s     489 MB/s
L4     VolumePool get (1 disk, buffered)                8 MB/s    1175 MB/s    1244 MB/s
L4     VolumePool get_stream (1 disk, mmap)              -        19098 MB/s    (page cache)
L4     VolumePool put_stream (4 disk, 3+1 EC)           2 MB/s     371 MB/s     409 MB/s
L4     VolumePool get (4 disk, buffered)                1 MB/s     774 MB/s     919 MB/s
L4     VolumePool get_stream (4 disk, mmap EC)           -         1048 MB/s    1236 MB/s
L5     HTTP transport (hyper, no S3)                   32 MB/s     762 MB/s     800 MB/s
L6     S3 PUT + storage (s3s, no SigV4)                 3 MB/s     272 MB/s     310 MB/s
L6     S3 GET + storage (mmap, 1 disk)                   -          809 MB/s    1048 MB/s
```

**What each layer tells you:**

- **L1** -- MD5 is 6.1x slower than blake3. Both are computed inline during
  the stream, so their cost overlaps with I/O for large objects.
- **L2** -- RS encode at 2762 MB/s is not a bottleneck. SIMD-accel is enabled.
- **L3** -- Disk write is the ceiling. 4KB is slow (filesystem metadata overhead).
- **L4** -- PUT at 439 MB/s vs L3 at 1625 MB/s = 3.7x overhead from hashing
  + RS + metadata writes. GET (1+0 mmap) is effectively instant for cached
  files -- 23 GB/s at 10MB, page-cache speed at 1GB. EC GET (4-disk) does
  1048-1236 MB/s via zero-alloc mmap fast path (slices directly from mmap
  when all shards healthy, no Vec allocation per block).
- **L5** -- Raw HTTP transport does 762 MB/s PUT at 10MB. HTTP itself is fast.
- **L6** -- s3s PUT = 272 MB/s at 10MB. The gap between L4 (439) and L6
  (272) is s3s dispatch overhead. s3s GET (1 disk, mmap): 809 MB/s at 10MB,
  1048 MB/s at 1GB. Direct curl test: 1220 MB/s at 1GB.

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

### Per-layer benchmark (L1-L6)

No external binaries needed. Tests all 6 layers at 4KB, 10MB, 1GB.
Saves JSON to `bench-results/` for A/B comparison. ~5 minutes total.

```bash
# run all layers at all sizes
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

Layers: L1 (hashing), L2 (RS encode), L3 (disk I/O), L4 (storage pipeline),
L5 (HTTP transport), L6 (S3 protocol + storage).

The comparison output flags regressions (>5% slower) and improvements (>5% faster)
per layer, per size, so you can see exactly what a change affected.
