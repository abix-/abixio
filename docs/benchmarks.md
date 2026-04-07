# Benchmarks

Windows 10, single machine, NTFS tmpdir, sequential requests, no concurrency.
All page-cache writes (no fsync). 10 iterations per measurement.

## Headline numbers

**10MB objects** (via `mc`, 1 disk, no EC):

```
PUT    AbixIO  147 MB/s    RustFS   77 MB/s    MinIO   88 MB/s
GET    AbixIO  143 MB/s    RustFS   99 MB/s    MinIO  124 MB/s
```

AbixIO PUT is 1.9x RustFS and 1.7x MinIO at this size.

**1GB objects** (via `mc`, 1 disk, no EC):

```
PUT    AbixIO  353 MB/s    RustFS  464 MB/s    MinIO  537 MB/s
GET    AbixIO  372 MB/s    RustFS  576 MB/s    MinIO  849 MB/s
```

AbixIO is slower at 1GB. The storage layer itself does 468 MB/s (see below),
so the gap is HTTP/s3s overhead, not the encode path.

**Metadata** -- HEAD, LIST (100 objects), DELETE are all 63-66ms across all
three servers. The `mc` process startup time (~63ms) is the floor here.

RustFS 1.0.0-alpha.90, MinIO RELEASE.2026-04-07 (archived).

## Storage layer (no HTTP)

VolumePool direct -- what the encode path actually does, without HTTP or S3
protocol overhead. This is where optimization work targets.

```
                  1 disk                          4 disks (3+1 EC)
            1MB    10MB    100MB    1GB      1MB    10MB    100MB    1GB
PUT stream  248    436     480      468      197    377     435      382   MB/s
GET         715   1164    1143      790       90    736     933      564   MB/s
```

PUT peaks around 100MB-1GB. GET peaks at 10-100MB then drops at 1GB (memory
pressure from full-object decode). 4-disk EC adds overhead from RS encode +
4x shard I/O on the same physical drive.

## Where the time goes (10MB PUT, 1 disk)

```
MD5 ETag          15ms   37%  -----> computed inline during stream (no separate pass)
HTTP + s3s        12ms   29%
tokio::fs::write   7ms   17%  -----> disk baseline, can't improve
RS encode 3+1      4ms   10%  -----> SIMD-accelerated (simd-accel feature)
blake3 checksums   3ms    7%  -----> computed inline during stream

server total      41ms
+ client SigV4    27ms         (not server-side, client hashes the body)
= end-to-end     68ms   = 147 MB/s
```

MD5 and blake3 are now computed inline during the streaming read, so their
cost overlaps with network I/O for large objects. The 15ms shown above is
the isolated CPU cost measured in the layer benchmark.

## Raw disk baseline

```
tokio::fs::write (cached)    1,287 MB/s    page cache, no fsync
tokio::fs::write + fsync       879 MB/s    real disk speed
tokio::fs::read  (cached)    2,574 MB/s    page cache
```

AbixIO PUT at 147 MB/s vs cached write at 1,287 MB/s = 8.8x overhead.
Most of that is MD5 + HTTP + S3 protocol, not the storage engine.

## Running benchmarks

```bash
# internal perf bench (storage layer, multi-size, saves JSON for A/B comparison)
cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# compare against a baseline
BENCH_COMPARE=bench-results/baseline.json cargo test --release --test layer_bench -- --ignored bench_perf --nocapture

# competitive benchmark (AbixIO vs RustFS vs MinIO via mc)
ABIXIO_BIN=./target/release/abixio RUSTFS_BIN=rustfs MINIO_BIN=minio bash tests/compare_bench.sh

# layer isolation (individual component timing)
cargo test --release --test layer_bench -- --ignored --nocapture
```

`BENCH_SIZES=10485760` to test one size. `ITERS=10` for stable numbers.
Any server binary can be omitted in compare_bench.sh -- that column shows "skip".
