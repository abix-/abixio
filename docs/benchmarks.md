# Benchmarks

Windows 10, single machine, NTFS tmpdir, sequential requests, page-cache writes.
10 iterations per measurement. RustFS 1.0.0-alpha.90, MinIO RELEASE.2026-04-07.

## PUT throughput (via mc)

![PUT throughput](img/bench-put.svg)

At 10MB, AbixIO PUT is 1.9x RustFS and 1.7x MinIO.
At 1GB, AbixIO trails -- the storage layer does 468 MB/s but HTTP/s3s overhead reduces it to 353.

## GET throughput (via mc)

![GET throughput](img/bench-get.svg)

Metadata ops (HEAD, LIST, DELETE) are 63-66ms across all three -- dominated by mc process startup.

## Where the time goes (10MB PUT)

![PUT breakdown](img/bench-breakdown.svg)

MD5 and blake3 are computed inline during the streaming read -- their cost
overlaps with network I/O for large objects. Client SigV4 is not server-side.

## Storage layer (no HTTP)

VolumePool direct. What the encode path does without HTTP overhead.

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

PUT 147 MB/s vs cached write 1,287 MB/s = 8.8x overhead (MD5 + HTTP + S3 protocol).

## Running benchmarks

```bash
# internal perf bench (saves JSON for A/B comparison)
cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# compare against a baseline
BENCH_COMPARE=bench-results/baseline.json cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# competitive benchmark (AbixIO vs RustFS vs MinIO)
ABIXIO_BIN=./target/release/abixio RUSTFS_BIN=rustfs MINIO_BIN=minio bash tests/compare_bench.sh
```
