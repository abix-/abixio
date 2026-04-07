# Benchmarks

## Setup

- Windows 10, single machine, all volumes on same NTFS drive (tmpdir)
- Single-node, 1 disk, no erasure coding (1+0)
- Sequential requests, no concurrency, page-cache writes (no fsync)
- 10 iterations per measurement
- `mc` client with UNSIGNED-PAYLOAD over HTTP
- AbixIO `ca601cf`, RustFS 1.0.0-alpha.90, MinIO RELEASE.2026-04-07

## PUT throughput

![PUT throughput](img/bench-put.svg)

At 4KB, all three servers are bottlenecked by `mc` process startup (~70ms).
At 10MB, throughput is comparable across servers (74-90 MB/s).
At 1GB, AbixIO (375 MB/s) trails RustFS (541) and MinIO (576). The storage
layer does 468 MB/s at 1GB (see below), so the gap is HTTP/s3s overhead.

## GET throughput

![GET throughput](img/bench-get.svg)

MinIO GET scales better at 1GB (801 MB/s) than both Rust servers.
At 10MB, all three are in the same range (96-122 MB/s).

## Where the time goes (10MB PUT)

![PUT breakdown](img/bench-breakdown.svg)

MD5 and blake3 are computed inline during the streaming read -- their cost
overlaps with network I/O for large objects. Client SigV4 is not server-side.

## Storage layer (no HTTP)

VolumePool direct, bypassing HTTP/s3s/mc. This is the encode path ceiling.

```
                  1 disk                          4 disks (3+1 EC)
            1MB    10MB    100MB    1GB      1MB    10MB    100MB    1GB
PUT stream  248    436     480      468      197    377     435      382   MB/s
GET         715   1164    1143      790       90    736     933      564   MB/s
```

## Raw disk baseline

```
tokio::fs::write (cached)    1,287 MB/s
tokio::fs::write + fsync       879 MB/s
tokio::fs::read  (cached)    2,574 MB/s
```

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

### Internal performance benchmark (storage layer)

No external binaries needed. Saves JSON to `bench-results/` for A/B comparison.

```bash
# establish baseline
cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# after making a change, compare
BENCH_COMPARE=bench-results/baseline.json \
    cargo test --release --test layer_bench -- --ignored bench_perf --nocapture
```

Tuning: `BENCH_SIZES=10485760` for one size, `BENCH_ITERS=20` for stability.
