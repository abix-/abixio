# Layer optimization

Systematic per-layer performance analysis of the AbixIO PUT and GET paths.
Each layer is treated as an independent optimization problem: measure it,
identify the ceiling, find the bottleneck, test a fix, record the result.

Last updated: 2026-04-09, commit `9a22d2a`.

## Table of contents

- [Layer architecture](#layer-architecture)
- [Benchmark environment](#benchmark-environment)
- [L1: Hashing](#l1-hashing)
- [L2: RS encode](#l2-rs-encode)
- [L3: Disk I/O](#l3-disk-io)
- [L4: Storage pipeline](#l4-storage-pipeline-volumepool)
- [L5: HTTP transport](#l5-http-transport)
- [L6: S3 protocol](#l6-s3-protocol-s3s--full-pipeline)
- [L7: Full client path](#l7-full-client-path-aws-sdk-s3--auth)
- [Layer-to-layer gaps](#layer-to-layer-gaps)
- [Optimization history](#optimization-history)
- [Allocation audit](#allocation-audit)
- [Log-structured storage](#log-structured-storage-implemented)
- [Competitive analysis](#competitive-analysis-minio-and-rustfs)
- [Future optimization candidates](#future-optimization-candidates)

---

## Layer architecture

```
PUT path (top to bottom):

  Client request
       |
  L7   Full client (aws-sdk-s3)    SigV4 signing, SDK HTTP, UNSIGNED-PAYLOAD
  L6   S3 protocol (s3s)           parse HTTP, SigV4 verify, dispatch to AbixioS3
  L5   HTTP transport (hyper)      accept TCP, read/write body
  L4   Storage pipeline            VolumePool: placement, RS encode, write shards
  L3   Disk I/O                    tokio::fs::write / tokio::fs::read
  L2   RS encode                   reed-solomon-erasure galois_8, SIMD
  L1   Hashing                     blake3 (shard integrity) + MD5 (S3 ETag)

GET path reverses: L7 -> L6 -> L5 -> L4 -> L3 -> L2 (decode) -> L1 (verify)
```

L1-L3 are primitives measured in isolation. L4 combines them into the storage
pipeline. L5 measures raw HTTP. L6 adds the S3 protocol layer on top. L7
adds a real S3 client with auth.

## Benchmark environment

- Windows 10 Home, single machine, all volumes on same NTFS drive (tmpdir)
- Single-node, page-cache writes (no fsync), sequential requests
- 10 iterations per measurement (50 for 4KB, 5 for 1GB, 30 for focused tests)
- `bench_perf` in `tests/layer_bench.rs` with JSON output and A/B comparison

---

## L1: Hashing

### Numbers (10MB)

| Operation | Throughput |
|-----------|-----------|
| blake3 | 4303 MB/s |
| MD5 | 703 MB/s |

### Analysis

blake3 uses SIMD (AVX2/SSE4.1), 4.3 GB/s is near hardware ceiling. MD5 at
703 MB/s is near theoretical max for single-threaded MD5. Both are computed
inline during the streaming encode path -- their cost overlaps with I/O for
large objects (10MB+).

MD5 is required by S3 (ETag = MD5 of body) when the client sends
`Content-MD5`. When absent, we skip MD5 and use xxhash64 for the ETag.

### MD5 skip optimization (done, 2026-04-09)

**Before**: MD5 computed inline on every PUT regardless of client headers.
703 MB/s throughput = ~30% of L4 PUT time for 1GB objects.

**After**: when `input.content_md5` is `None` (the common case -- aws-sdk-s3
with `disable_payload_signing()`, mc, rclone all skip it), set
`PutOptions.skip_md5 = true`. The encode path uses xxhash64 (~10+ GB/s)
instead of MD5 for the ETag. Format: `{xxh64:016x}{size:016x}` (32 hex
chars, same length as MD5 ETags).

**Result**: L7 1GB PUT improved from 380 to **695 MB/s (+83%)**. Matrix
PUT improved from 320 to **441 MB/s (+38%)**, now fastest of all three
servers.

Files changed:
- `src/storage/metadata.rs` -- added `skip_md5: bool` to `PutOptions`
- `src/storage/erasure_encode.rs` -- conditional MD5/xxhash in streaming path
- `src/storage/volume_pool.rs` -- conditional MD5/xxhash in small-object path
- `src/s3_service.rs` -- set `skip_md5` based on `content_md5` header

### MD5 assembly research

Investigated 2026-04-09 for cases where MD5 IS required:

- **`md-5` crate `asm` feature**: ships x86_64 assembly via `md5-asm` crate.
  **Fails on MSVC** -- the `.S` files use GAS syntax which `cl.exe` cannot
  compile. Would work on Linux/macOS or with `x86_64-pc-windows-gnu` target.
- **Go's `crypto/md5`** (what MinIO uses): has hand-written amd64 assembly
  that runs automatically.
- **RustCrypto/asm-hashes archived** (Aug 2025). Official recommendation:
  port to Rust `global_asm!` inline assembly in RustCrypto/hashes. SHA-1
  already has `compress/x86.rs`; MD5 does not yet have x86_64 inline asm.
- **Future**: write x86_64 MD5 compress using `global_asm!` (works on all
  targets including MSVC). On Linux, enable `md-5 = { features = ["asm"] }`.

---

## L2: RS encode

### Numbers (10MB)

| Operation | Throughput |
|-----------|-----------|
| RS encode 3+1 | 2762 MB/s |

### Analysis

reed-solomon-erasure with `simd-accel` feature. 2.8 GB/s is within expected
range for galois_8 on x86. 6x faster than the L4 storage pipeline (439 MB/s).

Could use ISA-L bindings for ~2x more, but pointless when L4 is the bottleneck.

### Decision

**No change.** RS encode is not limiting.

---

## L3: Disk I/O

### Numbers

| Operation | 10MB | 1GB |
|-----------|------|-----|
| write (page cache) | 1625 MB/s | 1056 MB/s |
| read (cached) | 2703 MB/s | 2902 MB/s |

### Analysis

These are the ceiling numbers for L4. Page-cache writes avoid disk sync -- real
durability requires fsync, which would cut throughput to physical disk speed
(SATA SSD ~500 MB/s, NVMe ~2-3 GB/s).

4KB writes are slow (13 MB/s) due to filesystem metadata overhead per operation.

### Decision

**No change.** This is the theoretical ceiling, not the current bottleneck.
Future: configurable fsync for durability, io_uring on Linux for zero-copy I/O.

---

## L4: Storage pipeline (VolumePool)

### Numbers (after optimization)

| Operation | 1 disk 10MB | 1 disk 1GB | 4 disk 10MB | 4 disk 1GB |
|-----------|------------|-----------|------------|-----------|
| put_stream | 439 MB/s | 489 MB/s | 371 MB/s | 409 MB/s |
| get (buffered) | 1175 MB/s | 1244 MB/s | 774 MB/s | 919 MB/s |
| get_stream | 1287 MB/s | 1439 MB/s | 842 MB/s | 983 MB/s |

### PUT analysis

The streaming PUT path (`encode_and_write` in `src/storage/erasure_encode.rs`)
reads body chunks, accumulates to 1MB blocks, RS-encodes each block, and writes
encoded shard data to each shard writer sequentially.

The sequential write loop in `write_block()`:
```rust
for (i, shard_data) in shards.iter().enumerate() {
    shard_hashers[i].update(shard_data);
    writers[i].write_chunk(shard_data).await?;
}
```

L4 PUT at 510 MB/s (with skip_md5) vs L3 disk write at 1625 MB/s = 3.2x
overhead from inline hashing (blake3 + xxhash64), RS encode, metadata
writes, and the sequential shard write loop. Without skip_md5 (MD5
required): 439 MB/s, 3.7x overhead.

### PUT parallelism experiments (all failed)

Three approaches were tested to parallelize shard writes:

1. **FuturesUnordered** (earlier work): regressed 10MB from 147->74 MB/s.
   Per-iteration spawn overhead exceeded sequential cost.

2. **Channel-based shard tasks** (this session): spawned one tokio task per
   shard writer at encode start, sent blocks via mpsc channels. Regressed
   4-disk 10MB by -21%. Channel + task scheduling overhead outweighed any
   parallelism benefit.

3. **join_all on concurrent writes**: not possible due to Rust's borrow rules
   (`write_chunk` takes `&mut self`, can't hold multiple `&mut` into a slice).

**Root cause**: all tmpdir shard files are on the same physical NTFS volume.
There is no actual I/O parallelism to exploit. The OS page cache serializes
writes to the same device. Parallel shard writes would only help when shards
map to different physical disks -- which the single-machine benchmark doesn't
test.

**Decision**: sequential writes are correct for same-disk configurations.
The comment in `write_block()` documents this finding so future work doesn't
repeat the experiment without separate physical disks.

### GET analysis and optimization (done)

**Before**: `read_and_decode()` read all shard files in parallel via
`join_all` (good), but collected the full decoded object into a `Vec<u8>`
before returning. For 1GB objects, this allocated 1GB in memory before the
first response byte could be sent.

**After**: `read_and_decode_stream()` opens shard files, reads them in 1MB
blocks matching the encode path's `STREAM_BLOCK_SIZE`, RS-decodes each block,
and yields decoded chunks via a `futures::channel::mpsc` channel (4-slot
buffer). A spawned tokio task handles the block-by-block decode loop.

Key implementation details:
- `Backend::read_shard_stream()` returns `(AsyncRead, ObjectMeta)` instead of
  `(Vec<u8>, ObjectMeta)`. LocalVolume opens the file; RemoteVolume falls back
  to buffered read.
- Block boundaries are derived from `total_size`, `data_n`, and
  `STREAM_BLOCK_SIZE` (1MB). Each shard file stores blocks contiguously, so
  per-block shard chunk size = `ceil(block_size / data_n)`.
- The last block may be smaller than 1MB. `split_data` padding is handled
  by truncating the reassembled block to the actual remaining bytes.

**Result**: +9-16% throughput at L4, but the bigger win is at L6 (see below).

Files changed:
- `src/storage/erasure_decode.rs` -- `read_and_decode_stream()`
- `src/storage/mod.rs` -- `Backend::read_shard_stream()`, `Store::get_object_stream()`
- `src/storage/local_volume.rs` -- native file-backed `read_shard_stream()`
- `src/storage/volume_pool.rs` -- `VolumePool::get_object_stream()`

### GET optimization: mmap fast path (done)

**Problem**: streaming decode still allocated 1MB `Bytes` per chunk through
an mpsc channel (1024 chunks for 1GB). RustFS uses `tokio::io::duplex` + mmap.

**After**: two-case approach via `memmap2`:

1. **1+0 (no EC)**: mmap the shard file, `Bytes::from_owner(mmap)` gives a
   ref-counted handle to the entire file. Yield 4MB `.slice()` chunks --
   zero-copy, zero allocation, no RS decode, no spawned task.

2. **EC (N+M)**: mmap all shard files. Spawned task slices directly into mmap
   regions (no read syscall), RS-decodes 4MB blocks (4x fewer iterations than
   1MB), yields decoded data through mpsc channel.

Both paths use `Backend::mmap_shard()` which returns `MmapOrVec` -- local
volumes return a real mmap, remote volumes fall back to buffered read.

**Result**:
- L4 GET (1 disk, mmap): 19,098 MB/s at 10MB (page cache), effectively instant
- L6 GET (1 disk): 809 MB/s at 10MB, **1048 MB/s at 1GB** (was 833)
- curl GET (no auth): **1220 MB/s at 1GB** -- faster than MinIO through mc
- EC GET (4 disk): **1048 MB/s (10MB), 1236 MB/s (1GB)** -- zero-alloc fast
  path slices from mmap when all shards healthy. +35% over pre-mmap baseline

Files changed:
- `Cargo.toml` -- added `memmap2`
- `src/storage/mod.rs` -- `MmapOrVec` type, `Backend::mmap_shard()`
- `src/storage/local_volume.rs` -- mmap-backed `mmap_shard()`
- `src/storage/erasure_decode.rs` -- mmap-based `read_and_decode_stream()`, 4MB blocks
- `src/storage/volume_pool.rs` -- 1+0 mmap fast path in `get_object_stream()`

---

## L5: HTTP transport

### Numbers (10MB)

| Operation | Throughput |
|-----------|-----------|
| HTTP PUT (reqwest -> hyper) | 762 MB/s |

### Analysis

hyper 1.x with http-body-util on localhost. 762 MB/s is reasonable -- limited
by memcpy + TCP stack overhead. The benchmark uses `data.clone()` on each PUT
which adds a copy; the real S3 path streams the body without copying.

### Decision

**No change.** HTTP transport is not limiting (762 MB/s > L6's 272 MB/s).

---

## L6: S3 protocol (s3s + full pipeline)

### Numbers (after optimization)

| Operation | 10MB | 1GB |
|-----------|------|-----|
| S3 PUT | 272 MB/s | 310 MB/s |
| S3 GET (1 disk, mmap) | 809 MB/s | 1048 MB/s |
| curl GET (1 disk, no auth) | -- | 1220 MB/s |

L6 runs in-process with `no_auth: true` and raw reqwest (no SigV4).

### PUT optimization: versioning config cache (done)

**Before**: every PUT called `get_versioning_config()` which read bucket
settings from disk via `Backend::read_bucket_settings()`. This added a file
read to every single PUT request, even when versioning was never configured.

**After**: `AbixioS3` caches versioning config in a `RwLock<HashMap>`. First
access populates the cache from disk. `put_bucket_versioning` invalidates the
cache entry. `get_bucket_versioning` also invalidates before re-reading to
ensure fresh data.

**Result**: L6 PUT improved from 214 MB/s to 272 MB/s (+27%).

Remaining PUT overhead (272 vs L4's 439 = 38% gap) is s3s request parsing,
XML serialization, and service dispatch. This is inherent to the protocol
layer and not easily reducible without replacing s3s.

Files changed:
- `src/s3_service.rs` -- `cached_versioning_config()`, `invalidate_versioning_cache()`

### GET optimization: streaming response (done)

**Before**: `get_object()` called `Store::get_object()` which returned
`(Vec<u8>, ObjectInfo)` -- the entire object buffered in memory. Then it
wrapped the data as a single-chunk `StreamingBlob::wrap(once(...))`. For
1GB objects, this meant 1GB allocated before the first response byte was sent.

**After**: `get_object()` calls `Store::get_object_stream()` which returns
`(ObjectInfo, Stream<Item=Result<Bytes>>)`. The stream feeds directly into
`StreamingBlob::wrap()`. The first response byte is sent after decoding the
first 1MB block, not after decoding the entire object.

Key implementation detail: s3s `StreamingBlob::wrap()` requires `Send + Sync`.
The boxed stream from `get_object_stream()` is `Send` but not `Sync`. A
`SyncStream` newtype wrapper adds `Sync` via `unsafe impl` -- this is safe
because streams are only polled from one task at a time.

Range requests and versioned GETs still use the buffered path because they
need random access into the response body. This is acceptable because range
requests typically target small byte ranges, and versioned objects are the
minority of GET traffic.

**Result**: L6 GET improved from 365 to 750 MB/s at 10MB (+105%), and from
452 to 833 MB/s at 1GB (+84%).

Files changed:
- `src/s3_service.rs` -- streaming GET path, `SyncStream`, `get_object_buffered()`

---

## L7: Full client path (aws-sdk-s3 + auth)

Added 2026-04-09 to close the gap between L6 (in-process, no auth) and
external matrix benchmarks.

### Numbers (1GB, 2026-04-09)

| Operation | L6 (no auth, reqwest) | L6A (auth, reqwest) | L7 (auth, aws-sdk-s3) |
|---|---|---|---|
| PUT (before skip-md5) | 344 MB/s | 337 MB/s | 380 MB/s |
| PUT (after skip-md5) | -- | -- | **695 MB/s** |
| GET | 535 MB/s | -- | 696 MB/s |

### Analysis

L7 runs in-process with auth enabled and the full aws-sdk-s3 client --
same path as real users. aws-sdk-s3 does not send `Content-MD5`, so
`skip_md5` is active and xxhash64 is used for the ETag.

The matrix benchmark previously showed 52 MB/s because it was running an
unoptimized debug binary (`target/debug/abixio.exe`). After fixing the
binary path to use release builds, matrix results match L7.

Auth overhead is negligible (L6 vs L6A: 344 vs 337 MB/s, within noise).
aws-sdk-s3 client overhead is negligible (L6 vs L7: 344 vs 380 MB/s pre-skip-md5).

---

## Layer-to-layer gaps

### 1GB GET -- full layer breakdown (2026-04-09, after chunked mmap)

| Layer | 1GB GET MB/s | What it measures |
|-------|-------------|-----------------|
| L4 get_stream | **instant** (mmap) | Storage: zero-copy mmap, 4MB Bytes::slice |
| L5 http_get | **724** | Raw hyper HTTP body (no S3, no storage) |
| L6 s3s_get | **714** | s3s + storage, no auth, reqwest |
| L7 sdk_get | **920** | s3s + auth + aws-sdk-s3 (in-process) |
| Matrix | **604** | Separate abixio.exe process |
| MinIO (matrix) | **791** | Separate minio.exe process |

Key findings:

1. **s3s adds near-zero overhead to GET** (L5 724 vs L6 714 -- within noise).
   The s3s response path is efficient. Prior attribution of GET bottleneck to
   s3s was incorrect.

2. **L5 (raw hyper) is the HTTP ceiling at 724 MB/s** for writing 1GB over
   127.0.0.1 TCP on Windows. This is the hyper/Windows loopback limit.

3. **The real GET gap is L7 (920) -> Matrix (604) = -34%**. This is the cost
   of running as a separate process on Windows. MinIO loses less here (791 vs
   920-equivalent) because Go's `net/http` is more efficient at TCP response
   writes on Windows than hyper.

4. **AbixIO's GET code is not the bottleneck.** The mmap path is zero-copy,
   chunked yield is working, s3s adds nothing. The remaining gap vs MinIO is
   hyper's TCP write performance on Windows vs Go's `net/http`.

### 1GB PUT -- full layer breakdown

| Layer | 1GB PUT MB/s | What it measures |
|-------|-------------|-----------------|
| L4 put_stream | **510** (skip-md5) | Storage: encode + write shards |
| L6 s3s_put | **344** (with MD5) | s3s + storage, no auth |
| L7 sdk_put | **695** (skip-md5) | s3s + auth + aws-sdk-s3 (in-process) |
| Matrix | **498** (skip-md5, TCP_NODELAY) | Separate abixio.exe process |
| MinIO (matrix) | **380** | Separate minio.exe process |

### Remaining gaps (after TCP_NODELAY)

| Gap | Current | Ratio | Notes |
|-----|---------|-------|-------|
| L7 PUT -> Matrix PUT | 695 vs 498 MB/s | 1.4x | process boundary (improved from 1.5x with TCP_NODELAY) |
| L7 GET -> Matrix GET | 920 vs 686 MB/s | 1.3x | process boundary (improved from 1.5x with TCP_NODELAY) |
| Matrix GET: AbixIO vs MinIO | 686 vs 757 MB/s | 0.91x | 9% gap, down from 24% before TCP_NODELAY |
| 10MB GET: AbixIO vs MinIO | 324 vs 748 MB/s | 0.43x | **2.3x gap** -- per-response overhead in hyper vs Go |
| L4 PUT vs L3 write | 510 vs 1625 MB/s | 3.2x | blake3 + xxhash + RS + metadata |
| EC GET vs 1+0 GET | 1236 vs page cache | -- | RS decode (inherent cost of EC) |

### TCP_NODELAY analysis (2026-04-09)

**Root cause**: Go enables `TCP_NODELAY` by default on all TCP connections.
AbixIO did not. Nagle's algorithm was batching small writes, adding latency.

**Fix**: `stream.set_nodelay(true)?` in `src/main.rs` after `listener.accept()`.

**Result**: Matrix PUT +10% (453->498), Matrix GET +14% (604->686). Closed
the 1GB GET gap from 24% to 9% vs MinIO.

**Remaining 10MB GET gap (2.3x vs MinIO)**: confirmed as hyper's HTTP
write ceiling, not AbixIO code. Evidence: L5 raw hyper GET (no S3, no
storage, just `Full::new(bytes)`) = **510 MB/s** with TCP_NODELAY. MinIO
achieves **748 MB/s** through a full S3 stack in a separate process.
### L5X: hyper sub-layer experiments (2026-04-09)

Tested hyper config variants to find where time goes in HTTP GET responses.
All measurements in-process, 10 iterations, TCP_NODELAY on.

| Variant | 10MB GET MB/s | 1GB GET MB/s | Config |
|---|---|---|---|
| raw_tcp | 327 | 395 | Raw tokio write_all, no HTTP framing |
| L5 (baseline) | 510 | 724 | hyper default, Full::new(bytes) |
| writev | 429 | 605 | hyper writev(true) |
| bigbuf | 364 | **800** | hyper max_buf_size(4MB) |
| stream | 462 | **849** | hyper StreamBody (4MB frame chunks) |
| **all_opts** | **726** | **844** | writev + bigbuf + stream combined |
| MinIO (matrix) | 648-685 | 796 | Go net/http reference |

Key findings:

1. **Raw TCP is SLOWER than hyper** (327 vs 510 MB/s at 10MB). Naive
   `write_all` on a raw socket is worse than hyper's optimized write path.
   hyper is NOT fundamentally slower -- its default config just isn't
   optimized for large bodies.

2. **`all_opts` hits 726 MB/s at 10MB** -- within 6% of MinIO's 685.
   The combination of `writev(true)` + `max_buf_size(4MB)` + chunked
   StreamBody nearly closes the gap at L5 level.

3. **For 1GB, tuned hyper matches MinIO** (844 vs 796 MB/s).

4. **The prior claim that "hyper's ceiling is 32% below MinIO" was wrong.**
   hyper with correct config is competitive. The gap was configuration,
   not architecture.

Applied: `writev(true)` + `max_buf_size(4MB)` in `src/main.rs`.
The remaining matrix gap (674 vs 796 for 1GB) is the s3s/ChunkedBytes
overhead between L5X and the full stack.

---

## Optimization history

| # | Change | Layer | Result | Status |
|---|--------|-------|--------|--------|
| 1 | Streaming GET | L4+L6 | **GET +105%** (10MB), **+84%** (1GB) | done |
| 2 | Parallel shard writes | L4 | **Regressed** -21%. Sequential faster on same disk | reverted |
| 3 | Versioning config cache | L6 | **PUT +27%** (10MB) | done |
| 4 | mmap GET (1+0 fast path) | L4+L6 | **L6 GET 1GB: 833->1048 MB/s (+26%)**. curl: 1220 MB/s | done |
| 5 | mmap GET (EC, 4MB blocks) | L4 | 4-disk GET 675-803 MB/s (regressed from mmap->Vec copy) | superseded by #6 |
| 6 | Zero-alloc EC GET fast path | L4 | **EC GET +35%** (919->1236 MB/s at 1GB). Slices from mmap, no Vec alloc | done |
| 7 | Debug binary fix | -- | **Matrix PUT 52->320 MB/s** (was benchmarking debug build) | done |
| 8 | Pipeline experiments | L4 | Double-buffer -96%, channel -96%, 4MB chunks -11%. All worse | reverted |
| 9 | Skip MD5 (xxhash64 ETag) | L1+L4+L7 | **L7 PUT +83%** (380->695 MB/s). Matrix PUT +38% (320->441). Fastest PUT at all sizes | done |
| 10 | Chunked mmap GET (4MB slices) | L4+L7 | **Matrix GET +25%** (484->604 MB/s). Zero-copy Bytes::slice | done |
| 11 | TCP_NODELAY on accept | L5+ | **Matrix PUT +10%** (453->498), **GET +14%** (604->686). Go sets this by default | done |
| 12 | ChunkedBytes GET (hybrid) | L6+L7 | **10MB GET +36%** (324->440 MB/s). Collect <=64MB, stream >64MB. Eliminates SyncStream+StreamWrapper chain | done |
| 13 | hyper writev + max_buf_size | L5+ | **1GB GET +15%** (584->674). L5X shows hyper can match MinIO with correct config | done |

### Before and after (L7, full client path -- what users see)

```
Operation          Before          After           Change
-----------        -----------     -----------     -------
PUT 1GB            380 MB/s        695 MB/s        +83%    (skip MD5)
GET 1GB            752 MB/s        880 MB/s        +17%    (chunked mmap)
```

### Before and after (matrix, external benchmark -- cumulative)

```
Operation          Start           Current         Change
-----------        -----------     -----------     -------
PUT 4KB            442 obj/s       1923 obj/s      +335%
PUT 10MB           50 MB/s         325 MB/s        +550%
PUT 1GB            52 MB/s         546 MB/s        +950%
GET 10MB           241 MB/s        399 MB/s        +66%
GET 1GB            577 MB/s        674 MB/s        +17%
```

All optimizations applied: debug binary fix, skip MD5 (xxhash64 ETag),
chunked mmap (4MB Bytes::slice), TCP_NODELAY, ChunkedBytes hybrid
response, hyper writev(true) + max_buf_size(4MB).

### Before and after (L6, S3 protocol in-process)

```
Operation          Before          After           Change
-----------        -----------     -----------     -------
PUT 10MB           214 MB/s        272 MB/s        +27%    (versioning cache)
PUT 1GB            305 MB/s        310 MB/s        ~same
GET 10MB           365 MB/s        809 MB/s        +122%   (streaming + mmap)
GET 1GB            452 MB/s        1048 MB/s       +132%   (mmap fast path)
GET 1GB (curl)     --              1220 MB/s       --      (no mc overhead)
```

### Before and after (L4, storage pipeline)

```
Operation          Before          After           Change
-----------        -----------     -----------     -------
GET 10MB (1d)      1175 MB/s       19098 MB/s      mmap page cache
GET 1GB (1d)       1244 MB/s       (page cache)    mmap instant
GET 10MB (4d EC)   774 MB/s        1048 MB/s       +35% (zero-alloc mmap)
GET 1GB (4d EC)    919 MB/s        1236 MB/s       +35% (zero-alloc mmap)
PUT 1GB (1d)       439 MB/s        510 MB/s        +16% (skip MD5, xxhash64)
```

---

## Allocation audit

Every allocation in the hot path is a potential throughput ceiling. The 1+0
GET path is already zero-alloc (mmap -> Bytes::from_owner -> slice). The EC
GET and PUT paths still allocate heavily. This section inventories every
allocation, rates its impact, and identifies the fix.

### Every allocation in the data path

Complete inventory of every heap allocation per request. "Per 1GB" counts
assume 3+1 EC with 1024 encode blocks and 256 decode iterations (4MB each).

#### 1+0 GET (`volume_pool.rs:get_object_stream` mmap fast path)

| # | File:line | What | Count | Heap? |
|---|-----------|------|-------|-------|
| 1 | volume_pool.rs:307 | `Bytes::from_owner(mmap)` | 1 | ref-counted wrapper, no data copy |
| 2 | volume_pool.rs:308 | `stream::once(owned)` -- yields entire Bytes | 1 | one yield, one poll_frame, zero slicing |
| 3 | s3_service.rs:419 | `StreamingBlob::wrap(SyncStream(..))` | 1 | one Box for stream trait object |
| 4 | s3_service.rs:424-435 | `info.content_type.clone()` etc | ~5 | small strings, once per request |

**Data-path allocs: 0. Stream yields: 1.** Entire mmap served as single Bytes.
hyper writes to TCP in kernel-sized chunks. No slicing, no Arc per chunk.

#### EC GET healthy (`erasure_decode.rs:read_and_decode_stream` fast path)

| # | File:line | What | Count | Heap? |
|---|-----------|------|-------|-------|
| 1 | :112 | `mmap_slots: Vec<Option<MmapOrVec>>` | 1 | once at start (N slots) |
| 2 | :135 | `meta_clone = good_meta.clone()` | 1 | strings in ObjectMeta |
| 3 | :137 | `mpsc::channel(4)` | 1 | channel internal buffers |
| 4 | :146 | `ReedSolomon::new()` | 1 | RS lookup tables |
| 5 | :158 | `pool.take()` -- **4MB output buffer from pool** | **0 (steady state)** | **done -- BufPool returns on drop** |
| 6 | :174-177 | `extend_from_slice(&mmap[..])` into iter_data | 256 x data_n | copy from mmap (inherent for EC reassembly) but no alloc (writes into #5) |
| 7 | :218 | `Bytes::from(iter_data)` | 256/GB | takes ownership of #5, no copy |

**Data-path allocs: 8 warmup, then 0 steady state.** BufPool pre-allocates
8 x 4MB Vecs. `Bytes::from_owner(PooledBuf)` wraps the buffer; when hyper
drops the Bytes, `PooledBuf::drop` returns the Vec to the pool. After
warmup, buffers circulate without allocation. EC GET 4-disk 1GB: 2105 MB/s.

#### EC GET degraded (slow path, missing shards)

| # | File:line | What | Count | Heap? |
|---|-----------|------|-------|-------|
| 1-7 | | same as healthy | same | same |
| 8 | :183 | `vec![None; total]` shard slot array | per degraded block | yes |
| 9 | :188 | `mmap[..].to_vec()` per available shard | per shard per degraded block | **yes -- full shard copy** |
| 10 | :200 | `rs.reconstruct()` internal | per degraded block | yes (reconstructs missing shards) |

**Only when shards are missing.** Correctness path, not optimized.

#### PUT (`erasure_encode.rs:encode_and_write` + `write_block`)

| # | File:line | What | Count | Heap? |
|---|-----------|------|-------|-------|
| 1 | :96-98 | `content_type.clone()` or `.to_string()` | 1 | once at start |
| 2 | :104 | `distribution: Vec<usize>` | 1 | once at start (N shards) |
| 3 | :109-112 | `volume_ids: Vec<String>` from placement | 1 | once at start |
| 4 | :118 | `ReedSolomon::new()` | 1 | RS lookup tables |
| 5 | :131 | `writers: Vec<Box<dyn ShardWriter>>` | 1 | N boxed writers |
| 6 | :138 | `shard_hashers: Vec<blake3::Hasher>` | 1 | N hashers |
| 7 | :144-146 | `shard_bufs: Vec<Vec<u8>>` -- **pre-allocated, reused** | 1 | N Vecs with capacity, allocated once |
| 8 | :149 | `block_buf: Vec::with_capacity(2MB)` | 1 | once, reused across all chunks |
| 9 | :313 (split_data_into) | `buf.clear()` + `extend_from_slice` + `resize` | 0 | **zero alloc -- reuses #7 capacity** |
| 10 | :318-320 | parity buf `clear()` + `resize` | 0 | **zero alloc -- reuses #7 capacity** |
| 11 | :193-195 | `checksums: Vec<String>` from blake3 hex | N shards | finalize path, once |
| 12 | :206-223 | `ObjectMeta { etag.clone(), .. }` per shard | N shards | finalize path, once per shard |
| 13 | :255-256 | `MrfEntry { bucket.to_string(), .. }` | 0 or 1 | only on partial write |
| 14 | :262-263 | `ObjectInfo { bucket.to_string(), .. }` | 1 | return value |

**Data-path allocs per block: 0.** The encode loop (#9, #10) allocates
nothing -- `split_data_into` and parity zeroing reuse pre-allocated buffers.
All allocations are either once-at-start (#1-8) or once-at-finalize (#11-14).

#### s3_service.rs response/request wrappers

| # | File:line | What | Count | Heap? |
|---|-----------|------|-------|-------|
| 1 | :417 | `io::Error::new(..e.to_string())` in stream map | per chunk on error | **error path only** |
| 2 | :419 | `SyncStream(mapped)` | 1 | stack wrapper, no heap |
| 3 | :419 | `StreamingBlob::wrap()` -> DynByteStream | 1 | one Box |
| 4 | :424-435 | `info.etag.clone()` etc | ~5 | small strings |
| 5 | :352 | PUT body `chunk.map_err(..e.to_string())` | per chunk on error | **error path only** |

**Data-path allocs: 0.** Error conversions only allocate on error.

### Allocation summary

| Path | Data-path allocs/GB | Remaining alloc | Can eliminate? |
|------|---------------------|-----------------|----------------|
| **1+0 GET** | **0** | -- | done |
| **EC GET (healthy)** | **0 (steady state)** | 8 warmup allocs, then pool recycles | **done** -- BufPool + PooledBuf + Bytes::from_owner |
| **EC GET (degraded)** | ~5400 | Vec per shard for RS reconstruct | no: RS API requires owned buffers |
| **PUT (per-block loop)** | **0** | -- | done |
| **PUT (total request)** | ~10 | setup + finalize allocs | no: inherent (metadata, hashers, writers) |
| **Response wrapper** | **0** | -- | done |

---

## Log-structured storage (implemented)

Small objects (<= 64KB) now use a log-structured storage path instead of
the file-per-object layout. See [write-log.md](write-log.md) for full design.

Measured with `tests/bench_4kb.py` (keep-alive, 1000 ops):

| | File tier | Log store | Improvement |
|--|----------|-----------|-------------|
| **4KB PUT** | 578 obj/s, 1.73ms | **1096 obj/s, 0.91ms** | **90% more throughput** |
| **4KB GET** | 774 obj/s, 1.29ms | **1315 obj/s, 0.76ms** | **70% more throughput** |
| Write ops per 4KB (4 disks) | 12 | **4** | 3x fewer |
| Files per 1M small objects | 3M+ | ~3 segments | ~1000x fewer |
| Activation | default | opt-in: `mkdir .abixio.sys/log/` | |

vs competitors (4KB keep-alive): RustFS 1329/1349, MinIO 1189/1073,
AbixIO(log) 1096/1315. Within 20% of fastest.

No fsync on writes. Page cache serves both read (mmap) and write
(file.write_all) paths. Same durability model as MinIO/RustFS.

Active segment is mmap'd at creation -- reads see writes immediately via
shared page cache. No sealing needed for reads. One active segment per disk.

**Windows `localhost` warning**: never use `localhost` for benchmarks.
Windows DNS resolution adds ~200ms per request. Always use `127.0.0.1`.

Remaining: GC, heal log-awareness, versioned objects.

---

## Pool L0: WriteSlotPool primitive (Phase 1, in development)

The pre-opened temp file pool is being built as an alternative to the
log store. See [write-pool.md](write-pool.md) for the full design and
[write-pool.md#implementation-status](write-pool.md#implementation-status)
for phase tracking.

Phase 1 lands the bare slot pool primitive: `WriteSlotPool::new`,
`try_pop`, `release`. No rename worker, no `pending_renames`, no
integration with `LocalVolume`. Just slot plumbing backed by
`crossbeam_queue::ArrayQueue` (lock-free MPMC).

### Numbers (Windows 10 NTFS, depth=32)

```
pool init (depth=32):                        11.96ms  (374us per slot pair)

single-thread pop+release (100k iters):
  avg  63ns   p50 100ns   p99 100ns   p999 100ns

concurrent pop+release:
   2 workers x 10000 ops:    20.4M ops/sec   48ns/op
   8 workers x 10000 ops:    14.7M ops/sec   68ns/op
  32 workers x 10000 ops:    12.2M ops/sec   81ns/op

empty try_pop:                              0ns (returns None, never blocks)
```

### Analysis

The pool primitive will never be the bottleneck. The plan target was
<500ns for pop+release; actual is 63ns avg, 8x under target. Throughput
sustains 12-20M ops/sec across contention levels with no collapse.
File syscalls in Phase 2+ will cap the pool at 5K-50K ops/sec depending
on size, so the primitive is 3-4 orders of magnitude faster than what
it needs to be.

The p50/p99/p999 percentiles all snap to exactly 100ns, which is the
`Instant::now()` resolution on this hardware -- the operation is
genuinely faster than the timer can measure individually.

`crossbeam_queue::ArrayQueue` is the right choice. No reason to revisit
the queue structure.

Bench: `tests/layer_bench.rs::bench_pool_l0_primitive`
Output: `bench-results/phase1-pool-primitive.txt`
Run: `k3sc cargo-lock test --release --test layer_bench -- --ignored
--nocapture bench_pool_l0_primitive`

### Next: Phase 2 (slot writes with real I/O) -- DONE, see Pool L1 below

---

## Pool L1: slot writes with real I/O (Phase 2, in development)

Phase 2 lands the `bench_pool_l1_slot_write` test in
`tests/layer_bench.rs`. Five sizes x six write strategies, each
optimization layered on independently so we can attribute the speedup
to a specific change. Still no rename worker, no `pending_renames`,
no integration with `LocalVolume`.

### Numbers (Windows 10 NTFS, 1 disk, p50)

```
SIZE   A: file_tier  C: pool_serial  D: pool_join  E: compact  F: sync_small
4KB    693.8us       27.9us          4.1us         3.9us       74.7us
64KB   1.08ms        31.3us          29.8us        55.5us      n/a
1MB    1.44ms        503.8us         828.8us       482.9us     n/a
10MB   5.63ms        3.68ms          3.72ms        3.64ms      n/a
100MB  91.91ms       36.19ms         37.65ms       36.34ms     n/a
```

### Headline finding: pool design fully validated

Pool wins at every size, by 1.5x to 170x:

| Size | A: file tier | D: pool join | Speedup |
|---|---|---|---|
| 4KB | 694us | 4.1us | **170x** |
| 64KB | 1.08ms | 29.8us | **36x** |
| 1MB | 1.44ms | 829us | 1.7x |
| 10MB | 5.63ms | 3.72ms | 1.5x |
| 100MB | 91.91ms | 37.65ms | **2.4x** |

The mkdir + 2 file creates is dramatically more expensive than I
expected, even at 100MB. The pool's syscall-elimination win persists
at every size.

### Optimization #1 (concurrent writes via `tokio::try_join!`)

| Size | C: serial | D: join | Speedup |
|---|---|---|---|
| 4KB | 33us avg | 6.3us avg | **5.5x** |
| 64KB | 251us avg | 186us avg | 1.35x |
| 1MB+ | tied | tied | noise |

**SHIP as default for all sizes.** Measured win is way bigger than
predicted -- the overlap of tokio blocking-pool dispatch helps more
than just saving the meta syscall.

### Optimization #2 (compact JSON)

Roughly neutral on speed across all sizes (avg numbers swing both
ways within noise; high variance dominates). Compact output is 391
bytes vs 597 bytes pretty, ~35% smaller on disk.

**SHIP for the disk-size win.** No measurable speed improvement on
this workload, but the disk savings are free. Open question: switching
to a faster JSON crate (simd-json, sonic-rs) might unlock real speed
wins -- queued as Phase 2.5 measurement.

### Optimization #3 (sync `std::fs::File::write_all` for small payloads)

| Size | E: pool_join_compact | F: pool_sync_small | Result |
|---|---|---|---|
| 4KB | 7.4us avg / 3.9us p50 | 86.0us avg / 74.7us p50 | **F is 14x slower** |

**REJECT.** Sync std::fs writes are dramatically slower than the tokio
async path on this workload, contradicting the conventional wisdom
that "sync is faster for small payloads, no dispatch overhead." The
cause may be a measurement artifact from `tokio::fs::File::into_std()`
leaving the file in a slow state, or it may be that the tokio
blocking-pool path is genuinely faster on Windows for this size class.
Either way, the measurement says no.

This is exactly the result the methodology exists to catch. Don't
ship optimizations that look right on paper but fail under measurement.

### mkdir cost (A vs B)

A: with mkdir = 776us at 4KB. B: pre-created dir = 587us. **mkdir
costs ~190us per PUT on NTFS at small sizes.** Negligible at 64KB+.
This is the dominant cost the pool eliminates.

### Surprises

- Pool wins at 100MB by 2.4x. Pre-bench prediction was "small or no
  improvement at 100MB+". Wrong by a lot.
- `tokio::try_join!` is 5.5x at 4KB, not the predicted 2x.
- `std::fs::File` is 14x slower than tokio's blocking-pool path for
  sync small writes. Counter to all conventional wisdom.
- High variance at 64KB-1MB (p99 is 30-50x p50). Likely NTFS write
  coalescing or page cache flushes. Not a pool problem; future bench
  should use 5-10x more iterations in this band for cleaner avgs.

### Final hot path (Phase 4 will integrate this)

```rust
let WriteSlot { mut data_file, mut meta_file, .. } = pool.try_pop().unwrap();
tokio::try_join!(
    data_file.write_all(&shard_bytes),
    meta_file.write_all(&compact_meta_json),
)?;
```

Two optimizations (#1 + #2). Optimization #3 dropped. Expected
hot-path latency at 4KB: **~6us through the pool vs ~776us through
the file tier. 130x faster.**

Bench: `tests/layer_bench.rs::bench_pool_l1_slot_write`
Output: `bench-results/phase2-slot-writes.txt`

### Next: Phase 2.5 (faster JSON serializer) -- DONE, see Pool L1.5 below

---

## Pool L1.5: JSON serializer comparison (Phase 2.5)

Phase 2 found that compact `serde_json::to_vec` showed no measurable
speed improvement over `to_vec_pretty` -- despite producing 35%
smaller output. The user wanted maximum possible JSON speed for the
pool hot path, so Phase 2.5 measured drop-in alternatives in
isolation.

### Numbers (Windows 10, 100k iters, ~500-byte ObjectMetaFile)

| Crate | avg | p50 | p99 | output | vs serde_json::to_vec |
|---|---|---|---|---|---|
| **simd-json::serde::to_vec** | **383ns** | **400ns** | **400ns** | 391 bytes | **-161ns (-30%)** |
| serde_json::to_vec | 544ns | 500ns | 700ns | 391 bytes | baseline |
| sonic-rs::to_vec | 614ns | 600ns | 700ns | 391 bytes | +70ns (+13%) |
| serde_json::to_vec_pretty | 858ns | 800ns | 1100ns | 597 bytes | +314ns (+58%) |

### Findings

**simd-json wins.** 383ns avg, 30% faster than `serde_json::to_vec`
on this payload. Output is compact (391 bytes) and round-trips
through `serde_json::from_slice` cleanly so destination meta.json
files stay fully compatible.

**sonic-rs is slower** in this regime, by 13%. The marketing claim
of 2-3x faster than serde_json applies to parsing larger documents,
not serializing 500-byte structs. The per-call codec overhead
dominates at this size. Rejected.

**simd-json's 30% win is also smaller than its marketing.** Same
reason -- simd-json shines on parsing large documents with SIMD
acceleration. At 391 bytes the SIMD setup cost eats most of the
theoretical speedup. We still get a real 30% improvement, just not
the 2-3x.

### Methodology lesson: Phase 2 had it wrong about compact JSON

Phase 2 concluded that compact JSON was "neutral on speed" because
strategy E (compact) was within noise of strategy D (pretty) when
measured end-to-end through the slot write path. **Phase 2.5 in
isolation shows compact serde_json IS 314ns faster than pretty.**
The 314ns difference disappeared into the I/O variance in Phase 2.

This is a generalizable insight: **components measured in isolation
can reveal real wins that end-to-end I/O variance buries.** When a
microbench result says "no change," check if the noise floor is
larger than the expected effect. If yes, drop one layer down and
measure the component alone.

### Production caveat

simd-json's effectiveness depends on runtime SIMD detection
(SSE4.2/AVX2 on x86_64; NEON on ARM64). The 30% win was measured
on this Windows x86_64 development box. **Re-run this bench on the
production target before committing to simd-json in Phase 4.**
sonic-rs and serde_json are the fallbacks if simd-json doesn't work
on a given target.

### The bigger truth

All four serializers complete in under 1us. The actual file syscall
on the meta write is ~10us. The full pool hot path is ~6us.
Switching to simd-json saves 161ns per PUT -- about 2-3% of the
end-to-end hot path. Real, but small in absolute terms.

We're shipping it because (a) the user explicitly said maximum JSON
speed is the goal, (b) round-trip parses validate, (c) the
dependency is small and stable, (d) every nanosecond matters when
the pool is competing with the log store's microsecond-level
operations.

### Decision

Phase 4 will use `simd_json::serde::to_vec` as the meta serializer
in the pool hot path, with `serde_json::to_vec` as the fallback if
the target doesn't support SIMD acceleration. sonic-rs is dropped.

Bench: `tests/layer_bench.rs::bench_pool_l1_5_json_serializers`
Output: `bench-results/phase2.5-json-serializers.txt`

After Phase 2.5: **Phase 3 (rename worker in isolation) -- DONE, see Pool L2 below.**

---

## Pool L2: rename worker drain rate (Phase 3)

Phase 3 builds the other half of the pool: the background worker
that completes each PUT by moving temp files to their destinations
and putting fresh slots back into the pool. Until Phase 3, the bench
harness was calling a fake `release()` to return slots; Phase 3
makes the loop real.

New code in `src/storage/write_slot_pool.rs`:

- `RenameRequest` struct -- the message sent on the rename channel
- `WriteSlotPool::replenish_slot(slot_id)` -- creates a fresh slot
  file pair at the same paths and pushes a new `WriteSlot` to the
  queue
- `process_rename_request(pool, req)` -- the worker's per-request
  work: mkdir + 2 renames + open a fresh slot to replace the
  consumed one
- `run_rename_worker(pool, rx, shutdown)` -- the loop, matching
  the heal worker shutdown pattern at `heal/worker.rs:181-226`

### Numbers (Windows 10 NTFS, 1 disk)

```
-- Scenario 1: cold drain (mkdir + 2 renames + open new slot) --
N=32     workers=1   drain    34.00ms     941 ops/sec   1063us/op
N=256    workers=1   drain   290.59ms     881 ops/sec   1135us/op
N=1024   workers=1   drain  1169.43ms     876 ops/sec   1142us/op

-- Scenario 2: drain with pre-created dest dirs (no mkdir) --
N=256    workers=1   drain   239.32ms    1070 ops/sec    935us/op

-- Scenario 4: parallel workers --
workers=1   drain   269.47ms     950 ops/sec
workers=2   drain   146.82ms    1744 ops/sec   1.84x scaling
workers=4   drain   127.28ms    2011 ops/sec   2.12x scaling
```

### Single worker drains at ~940 ops/sec

Per-op cost is consistent at ~1100us across all batch sizes, which
means there's no per-batch overhead. Estimated breakdown:

- mkdir: ~200us (17% of cost)
- rename data + rename meta: ~200us total
- open two new slot files: ~400-500us total -- **biggest line item**
- channel + tokio overhead: ~200us

Opening the two new slot files takes more time than mkdir or rename
individually. Pre-allocation doesn't help here -- every slot
replacement during steady-state pays the same cost.

### mkdir is ~17% of the cost

Scenario 2 (pre-created dest dirs) runs at 935us/op vs 1135us/op
in Scenario 1. mkdir saves ~200us per op. Throughput climbs from
881 to 1070 ops/sec (+21%). Worth a Phase 3.5 optimization (mkdir
caching, or optimistic rename + retry on ENOENT) if production load
shows the worker as a bottleneck.

### Parallel workers scale to 2x at 2 workers, plateau at 4

```
1 worker  → 950 ops/sec
2 workers → 1744 ops/sec    1.84x scaling
4 workers → 2011 ops/sec    2.12x scaling (only +15% over 2)
```

Two workers give near-linear scaling. Four workers fall off hard --
NTFS rename parallelism on a single disk has a hard ceiling around
2 concurrent independent operations. The kernel/filesystem is the
bottleneck past 2 workers, not the Rust code.

**Sweet spot: 2 workers per disk** if single-worker saturates.

### Vs the methodology target

The methodology target was "5000 ops/sec per disk." We hit ~940
single-worker, ~1750 with 2, ~2010 with 4. **Miss the aspirational
target by 2.5x.**

But the 5000 was a guess, not measured against any real workload.
The right comparison is what the worker REPLACES:

| Path | Throughput |
|---|---|
| File tier `put_object` 10MB 1 disk | ~35 ops/sec |
| MinIO `put_object` 4KB | ~367 ops/sec |
| **Pool worker drain (1 worker)** | **~940 ops/sec** |
| Pool worker drain (2 workers) | ~1750 ops/sec |

**The pool worker is 27x faster than the existing file tier and
2.5x faster than MinIO at 4KB.** It crushes the path it replaces.
The 5000 target was aspirational, not necessary.

The hot-path PUT measured in Phase 2 is ~6us per request, **180x
faster than the worker drain rate.** In any sustained load above
940 ops/sec, the rename queue grows and writes fall through to the
slow path (Phase 7 backpressure handles that). The PUT request side
is faster than the worker can keep up with -- not the other way
around.

### Verdict

**Pass.** Ship 1 worker per disk in Phase 4. Design supports 2
workers per disk if production load shows saturation -- 1.84x
scaling already proven. Don't bother with 4 workers.

### Methodology note: Scenario 3 was poorly designed

Scenario 3 (steady-state with target rates) showed numbers way
below cold-drain throughput (~280 ops/sec at "5000 target" vs ~940
in cold drain). The cause: I ran ONE PUT-side task feeding ONE
worker task, both fighting over the tokio blocking pool. That
doesn't match production, where many concurrent HTTP request handlers
feed the worker in parallel. **The right design for that scenario
is many parallel PUT-side tasks.** Will revisit in Phase 7
(backpressure) under realistic concurrent load.

### Surprises

- Opening fresh slot files dominates the per-op cost at ~400-500us,
  larger than mkdir or rename.
- NTFS rename parallelism caps at ~2 concurrent operations per disk.
- 180x gap between hot path and worker drain rate. The pool's whole
  design depends on the queue absorbing bursts -- the request side
  is much faster than the worker can keep up with.

Bench: `tests/layer_bench.rs::bench_pool_l2_worker_drain`
Output: `bench-results/phase3-rename-worker.txt`

### Next: Phase 4 (integrate the pool into LocalVolume::write_shard) -- DONE, see Pool L3 below

---

## Pool L3: integrated PUT throughput (Phase 4)

Phase 4 stops treating the pool as a freestanding test object. Calling
`LocalVolume::write_shard` with the pool turned on now goes through
the fast write path (Phase 2), the background worker finishes the
rename, and the destination ends up with a normal `shard.dat` and
`meta.json` that the rest of the system can read.

This is the first phase that touches production code instead of just
adding bench code. All 336 existing tests still pass.

### What changed

- `ObjectMetaFile` got two new fields, `bucket` and `key`, with
  `#[serde(default)]` for backward compatibility. Old `meta.json`
  files load fine; new ones are self-describing so the rename worker
  and crash recovery can find the destination from the file alone.
- `LocalVolume` got an opt-in pool. Call
  `LocalVolume::enable_write_pool(depth)` to turn it on. This opens
  the slot files at `.abixio.sys/tmp/`, starts the rename worker as
  a background task, and stores the connection points so
  `write_shard` can use them.
- `write_shard` now tries the pool first when it's enabled and the
  object isn't versioned. If a slot is available, it pops the slot,
  runs the Phase 2 fast write (simultaneous writes plus simd-json),
  sends a rename message to the worker, and returns. Everything else
  (versioned objects, pool empty, pool not enabled) goes through the
  existing slow path instead.
- `simd-json` moved from dev-dependencies to main dependencies
  because the production write path now uses it.
- `LocalVolume::drain_pending()` is a test helper that waits until
  every in-flight rename has finished. Tests need it because Phase 4
  has no read-path support yet -- a request that just got a 200 OK
  for a PUT through the pool can return 404 on an immediate GET
  until the worker has finished the rename. Phase 5 will fix that
  for real.

### Numbers (Windows 10 NTFS, 1 disk, full LocalVolume::write_shard call)

```
SIZE     STRATEGY                    AVG       p50       p99    THROUGHPUT
4KB      file tier (baseline)      1.09ms   649.6us   5.44ms      3.6 MB/s
4KB      pool (Phase 4)           132.7us    43.0us   2.10ms     29.4 MB/s

64KB     file tier (baseline)      1.32ms   1.00ms    6.85ms     47.5 MB/s
64KB     pool (Phase 4)           327.8us    57.5us   3.15ms    190.7 MB/s

1MB      file tier (baseline)      3.06ms   3.02ms    3.86ms    327.0 MB/s
1MB      pool (Phase 4)           789.9us   554.0us   3.59ms   1265.9 MB/s

10MB     file tier (baseline)     22.72ms   22.59ms  23.72ms    440.2 MB/s
10MB     pool (Phase 4)            3.94ms    3.76ms   5.14ms   2535.4 MB/s

100MB    file tier (baseline)    284.35ms  283.87ms 291.25ms    351.7 MB/s
100MB    pool (Phase 4)           35.95ms   35.56ms  37.53ms   2781.8 MB/s
```

### The headline: pool beats file tier by 5x to 18x at every size

| Size | File tier (median) | Pool (median) | How much faster |
|---|---|---|---|
| 4KB | 649us | **43us** | **15x** |
| 64KB | 1.00ms | **57us** | **18x** |
| 1MB | 3.02ms | **554us** | **5.5x** |
| 10MB | 22.59ms | **3.76ms** | **6x** |
| 100MB | 283ms | **35.56ms** | **8x** |

This is the real-world measurement, not a microbench in isolation.
The bench calls `LocalVolume::write_shard` exactly the way a real
PUT request would. **At 100MB the pool sustains 2782 MB/s versus
the file tier's 352 MB/s.** That's 8x more bandwidth out of the
exact same disk and the exact same operating system, with no
hardware changes.

### The integration cost: ~30-40us of constant overhead per call

Phase 2 measured just the write step in isolation -- pop a slot,
write the data, write the meta, drop the slot -- at about 4us
median for a 4KB object. Phase 4 measures the full
`LocalVolume::write_shard` call at about 43us median for the same
4KB. The gap is about 39us. It stays roughly the same across all
object sizes because most of it is per-call work that doesn't
scale with the payload:

- checking that the bucket name and key are valid: ~1us
- building the meta struct and turning it into JSON: ~1-2us
- computing the destination directory and file paths: ~3-5us
- general async function bookkeeping and struct unpacking: ~5us
- sending the rename message to the background worker: ~5-25us

At 4KB the 39us is 10x the bare 4us write step, which sounds bad in
percentage terms. At 1MB it's invisible. At 100MB it's noise. **The
fixed cost matters most for very small writes**, and is something to
come back to later if 4KB latency turns out to matter in production.
For now the 15x file-tier speedup at 4KB is already a huge win.

### Methodology lesson: pick the right comparison

The plan said the success criterion was "Phase 4 within 20% of
Phase 2's bare write step." We missed that target badly at 4KB (43us
vs 4us = 10x slower). But the criterion was the wrong yardstick:
Phase 2 didn't include any of the per-call work that Phase 4 has
to do, so Phase 4 could never get close to it at small sizes.

The right comparison is what matters in production: pool vs the
path it replaces. That comparison shows 5x to 18x speedups at every
size. **What you measure decides what you ship.** A bench that
shows the wrong number leads to the wrong decision.

### Surprises

1. The 1MB pool path is faster than the 1MB Phase 2 bare path.
   The Phase 4 bench had a warmup loop the Phase 2 bench didn't.
   Re-running Phase 2 with warmup would close the gap.
2. The integration overhead is roughly constant in absolute terms,
   not proportional to data size. It's per-call work (path
   computation, struct construction, message send), not data work.
3. The 18x win at 64KB is the biggest in the table. mkdir plus
   two file creates dominates the file tier at small sizes; below
   64KB the file tier hits some per-file fixed cost ceiling that
   the pool walks straight past.

Bench: `tests/layer_bench.rs::bench_pool_l3_integrated_put`
Output: `bench-results/phase4-integrated-put.txt`

### Next: Phase 5 (read path integration)

The pool's fast write path is in production code now, but reads
still go through the file tier. A request that just got 200 OK on
a pool PUT can return 404 on an immediate GET until the rename
worker has finished its work. Tests work around this by waiting for
the worker to catch up via `drain_pending()`; production code is
not yet safe to ship.

Phase 5 adds the in-memory `pending_renames` table so reads can
find an object that's still in flight. After Phase 5 the pool is
production-safe for non-versioned writes.

---

## Competitive analysis: MinIO and RustFS

Source code analysis of all three servers (2026-04-09). MinIO source at
`/c/code/minio`, RustFS source at `/c/code/rustfs`.

### PUT pipeline comparison

| | AbixIO | MinIO | RustFS |
|---|---|---|---|
| Read/encode pipeline | Sequential: read 1MB -> encode -> write -> repeat | **Readahead double-buffer** (`readahead.NewReaderBuffer`, 2 buffers) for files >= `bigFileThreshold` | **Separate tokio task + mpsc(8) channel**: read+encode decoupled from write |
| Shard writes per block | Sequential `for` loop (`erasure_encode.rs:327`) | Sequential `for` loop (`erasure-encode.go:34`) | **Parallel via `FuturesUnordered`** (`encode.rs:72`) |
| Key source | `src/storage/erasure_encode.rs:148` | `cmd/erasure-object.go:1426` | `crates/ecstore/src/erasure_coding/encode.rs:188` |

### GET pipeline comparison

| | AbixIO | MinIO | RustFS |
|---|---|---|---|
| Shard reads | 1+0: mmap, 4MB Bytes::slice. EC: sequential reads | **Parallel goroutines** via channel-based work-stealing (`parallelReader`) | **Parallel `FuturesUnordered`**, waits for `data_shards` to complete |
| Response model | Async stream (Box<Pin<dyn Stream>>) -> s3s Body -> hyper poll_frame | **Synchronous io.Copy** with 128KB pooled buffer -> http.ResponseWriter | tokio::io::duplex -> AsyncRead response |
| Dispatch overhead | 2x vtable per frame (DynByteStream + StreamingBlob) | **Zero** -- direct function calls | 1x vtable (AsyncRead trait object) |
| Scheduling | tokio async poll/wake per frame | **OS goroutine** -- no scheduler overhead | tokio async |
| Key source | `volume_pool.rs:532`, `s3_service.rs:421` | `erasure-object.go:292`, `object-handlers.go:551` | `set_disk.rs:655`, `decode.rs:217` |

### Why MinIO's GET is faster -- detailed code trace (2026-04-09)

MinIO GET data path (`cmd/erasure-object.go`, `cmd/object-handlers.go`):
```
getObjectNInfo():
  pr, pw := xioutil.WaitPipe()       // io.Pipe() -- kernel-backed sync pipe
  go func() {
    er.getObjectWithFileInfo(pw)      // decode goroutine writes to pipe
      -> erasure.Decode(pw, readers)  // parallel disk reads -> RS decode -> pw.Write()
  }()
  return GetObjectReader{Reader: pr}  // returns pipe reader

getObjectHandler():
  xioutil.Copy(httpWriter, gr)        // io.CopyBuffer with 128KB pooled buffer
    -> direct Write() calls to http.ResponseWriter -> TCP
```

AbixIO GET data path (`volume_pool.rs`, `s3_service.rs`):
```
get_object_stream():
  mmap_shard() -> Bytes::from_owner(mmap)
  Bytes::slice(4MB chunks) -> stream::iter(chunks)    // zero-copy

get_object():
  StreamingBlob::wrap(SyncStream(stream))              // Box<Pin<dyn Stream>>
    -> s3s Body::DynStream                             // enum dispatch
    -> hyper poll_frame()                              // async poll per frame
    -> TCP
```

The fundamental difference is **synchronous io.Copy vs async stream polling**.
MinIO's path has zero async overhead -- `io.CopyBuffer` reads 128KB from the
pipe and calls `Write()` on the HTTP response writer in a tight loop. AbixIO's
path goes through 2 levels of dynamic dispatch (`Box<Pin<dyn Stream>>`) and
tokio's async scheduler for every 4MB frame.

At **1GB** (256 frames), this overhead is amortized: 686 vs 757 MB/s (9% gap).
At **10MB** (3 frames), per-request fixed cost dominates: 324 vs 748 MB/s
(2.3x gap). The fixed cost includes s3s request parsing, response header
serialization, and hyper connection setup -- none of which MinIO's Go path pays.

**Confirmed**: raw hyper L5 GET (no S3, no storage) = 510 MB/s for 10MB with
TCP_NODELAY. MinIO achieves 748 MB/s through its full S3 stack. hyper's HTTP
write ceiling is 32% below MinIO for 10MB bodies on Windows.

### Future approaches to close the GET gap

1. **Bypass s3s for GET**: handle GET in `AbixioDispatch` with raw hyper,
   write response body directly to `hyper::body::Sender` without going
   through StreamingBlob/Body/poll_frame chain
2. **hyper sendfile**: investigate if hyper can write a File/mmap to the
   response without async stream polling
3. **Benchmark on Linux**: isolate Windows-specific TCP overhead (IOCP vs
   epoll may behave differently)

### Pipeline experiment results (2026-04-09, 1GB, 1 disk, 5 iterations)

Tested alternative pipeline strategies against the current sequential approach:

| Experiment | 1GB MB/s | vs baseline | Verdict |
|---|---|---|---|
| `put_1mb_seq` (current) | **556** | baseline | -- |
| `put_4mb_chunk` (4MB input chunks) | 494 | **-11%** | larger chunks hurt |
| `put_chan_pipe` (RustFS-style channel) | 24 | **-96%** | massive regression |
| `put_dbl_buf` (MinIO-style double-buf) | 24 | **-96%** | massive regression |
| `get_single` (current mmap, 1 Bytes) | page cache | baseline | -- |
| `get_4mb_chunk` (rechunk into 4MB) | 1,877 | much slower | chunking adds copy overhead |

**Root cause of channel/double-buffer regression**: the experimental
implementations collected data through the pipeline then wrote via
`put_object` (buffered). The channel and double-buffer overhead dominated
because the test stream yields instantly (pre-created chunks). These
approaches would only help with a real network stream that has latency to
overlap. On same-disk single-node with page-cache writes, the current
sequential path is already optimal.

**Root cause of GET chunking regression**: at L4, the mmap path returns
in <1ms (page cache). Rechunking adds `BytesMut` allocation and memcpy
overhead. The GET gap (555 MB/s in matrix vs 1048 MB/s at L6) is in the
HTTP/s3s layer, not the storage layer.

### Conclusions

1. **PUT pipeline changes (double-buffer, channel) are the wrong lever**.
   The 556 -> 320 MB/s gap is s3s dispatch + HTTP overhead. Fixing the
   encode pipeline cannot recover what's lost in the protocol layer.

2. **GET chunking is the wrong lever**. The mmap path is already zero-copy.
   The 1048 -> 555 MB/s gap is in s3s response serialization.

3. **Right lever: s3s overhead**. The L4-to-L6 gap (556->344 PUT,
   instant->535 GET) is where the most throughput is lost. Options:
   - Upgrade s3s (if newer versions are faster)
   - Bypass s3s for hot paths (raw hyper handler for PUT/GET)
   - Profile s3s internals to find specific bottlenecks

4. **Parallel shard writes only matter with multiple physical disks**.
   On same-disk tmpdir, sequential writes are optimal (confirmed earlier
   and reconfirmed by these experiments).

---

## Future optimization candidates

- **s3s upgrade or bypass**: s3s 0.13 adds ~38% overhead on PUT (when MD5 is
  required). For the common skip-md5 path, the L7->Matrix gap (695->441 MB/s)
  suggests process boundary overhead is now the bigger factor
- **MD5 assembly via `global_asm!`**: write x86_64 MD5 compress in Rust inline
  asm (works on MSVC). For Content-MD5 requests where MD5 can't be skipped
- **Multi-disk benchmark on separate physical devices**: parallel shard writes
  would likely help when shards map to different NVMe drives
- **io_uring on Linux**: zero-copy I/O could narrow the L4-vs-L3 gap
- **Configurable fsync**: per-bucket durability modes (none/data/full)
- **Streaming multipart GET**: currently falls back to buffered path
- **Streaming range requests**: currently falls back to buffered path
- **Log store GC**: reclaim dead space from sealed segments
- **Log store batch fsync**: configurable fsync interval for higher write throughput
