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
hyper's ceiling is 32% below MinIO's throughput for 10MB bodies.

This is a hyper/tokio/Windows TCP stack limitation, not something we can
fix in AbixIO's code. Options to investigate:
- Benchmark on Linux to isolate Windows-specific TCP overhead
- hyper HTTP/2 (may reduce per-response framing overhead)
- SO_SNDBUF tuning (larger TCP send buffer)
- Profile hyper response write path for unnecessary copies
- Consider a custom response writer that bypasses hyper for large GETs

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
PUT 4KB            442 obj/s       1815 obj/s      +311%   (debug fix + skip MD5 + TCP_NODELAY)
PUT 10MB           50 MB/s         284 MB/s        +468%   (debug fix + skip MD5 + TCP_NODELAY)
PUT 1GB            52 MB/s         498 MB/s        +858%   (debug fix + skip MD5 + TCP_NODELAY)
GET 1GB            577 MB/s        686 MB/s        +19%    (chunked mmap + TCP_NODELAY)
```

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
| Shard reads | 1+0: mmap, single `Bytes` yield. EC: sequential reads | **Parallel goroutines** via channel-based work-stealing (`parallelReader`) | **Parallel `FuturesUnordered`**, waits for `data_shards` to complete |
| Key source | `src/storage/volume_pool.rs:527` | `cmd/erasure-decode.go:127` | `crates/ecstore/src/erasure_coding/decode.rs:86` |

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
