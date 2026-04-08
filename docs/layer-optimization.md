# Layer optimization

Systematic per-layer performance analysis of the AbixIO PUT and GET paths.
Each layer is treated as an independent optimization problem: measure it,
identify the ceiling, find the bottleneck, test a fix, record the result.

Last updated: 2026-04-07, commit `ad0506a`.

## Layer architecture

```
PUT path (top to bottom):

  Client request
       |
  L6   S3 protocol (s3s)        parse HTTP, SigV4, dispatch to AbixioS3
  L5   HTTP transport (hyper)    accept TCP, read/write body
  L4   Storage pipeline          VolumePool: placement, RS encode, write shards
  L3   Disk I/O                  tokio::fs::write / tokio::fs::read
  L2   RS encode                 reed-solomon-erasure galois_8, SIMD
  L1   Hashing                   blake3 (shard integrity) + MD5 (S3 ETag)

GET path reverses: L6 -> L5 -> L4 -> L3 -> L2 (decode) -> L1 (verify)
```

L1-L3 are primitives measured in isolation. L4 combines them into the storage
pipeline. L5 measures raw HTTP. L6 adds the S3 protocol layer on top.

## Benchmark environment

- Windows 10 Home, single machine, all volumes on same NTFS drive (tmpdir)
- Single-node, page-cache writes (no fsync), sequential requests
- 10 iterations per measurement (50 for 4KB, 5 for 1GB, 30 for focused tests)
- `bench_perf` in `tests/layer_bench.rs` with JSON output and A/B comparison

---

## Before and after

Summary of all optimizations applied in this round of work.

### L6 (end-to-end S3 path) -- what users see

```
Operation          Before          After           Change
-----------        -----------     -----------     -------
PUT 10MB           214 MB/s        272 MB/s        +27%    (versioning cache)
PUT 1GB            305 MB/s        310 MB/s        ~same
GET 10MB           365 MB/s        809 MB/s        +122%   (streaming + mmap)
GET 1GB            452 MB/s        1048 MB/s       +132%   (mmap fast path)
GET 1GB (curl)     --              1220 MB/s       --      (no mc overhead)
```

### L4 (storage pipeline) -- internal

```
Operation          Before          After           Change
-----------        -----------     -----------     -------
GET 10MB (1d)      1175 MB/s       19098 MB/s      mmap page cache
GET 1GB (1d)       1244 MB/s       (page cache)    mmap instant
GET 10MB (4d EC)   774 MB/s        1048 MB/s       +35% (zero-alloc mmap)
GET 1GB (4d EC)    919 MB/s        1236 MB/s       +35% (zero-alloc mmap)
PUT (all sizes)    unchanged       unchanged       0%
```

EC GET fast path slices directly from mmap into output buffer when all data
shards are healthy (common case). Zero Vec allocation per block. Only falls
back to Vec + RS reconstruct when shards are actually missing.

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

MD5 is required by S3 (ETag = MD5 of body). Cannot be removed.

### Decision

**No change.** Hashing is not the bottleneck at any layer above it.

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

L4 PUT at 439 MB/s vs L3 disk write at 1625 MB/s = 3.7x overhead from inline
hashing (MD5 + blake3), RS encode, metadata writes, and the sequential shard
write loop.

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

## Optimization results summary

| # | Change | Layer | Result | Status |
|---|--------|-------|--------|--------|
| 1 | Streaming GET | L4+L6 | **GET +105%** (10MB), **+84%** (1GB) | done |
| 2 | Parallel shard writes | L4 | **Regressed** -21%. Sequential faster on same disk | reverted |
| 3 | Versioning config cache | L6 | **PUT +27%** (10MB) | done |
| 4 | mmap GET (1+0 fast path) | L4+L6 | **L6 GET 1GB: 833->1048 MB/s (+26%)**. curl: 1220 MB/s | done |
| 5 | mmap GET (EC, 4MB blocks) | L4 | 4-disk GET 675-803 MB/s (regressed from mmap->Vec copy) | superseded by #6 |
| 6 | Zero-alloc EC GET fast path | L4 | **EC GET +35%** (919->1236 MB/s at 1GB). Slices from mmap, no Vec alloc | done |

## Remaining gaps

| Gap | Current | Ceiling | Ratio | Notes |
|-----|---------|---------|-------|-------|
| L6 PUT vs L4 PUT | 272 vs 439 MB/s | 1.6x | s3s dispatch overhead |
| L4 PUT vs L3 write | 439 vs 1625 MB/s | 3.7x | hashing + RS + metadata |
| L6 GET (mc) vs L6 GET (curl) | 354 vs 1220 MB/s | 3.4x | mc client overhead (SigV4, Go HTTP) |
| EC GET vs 1+0 GET | 1236 vs page cache | -- | RS decode (inherent cost of EC) |

The largest remaining gap is the `mc` client bottleneck. AbixIO serves 1GB at
1220 MB/s via curl, but `mc` can only consume 354 MB/s. MinIO achieves
970 MB/s through `mc` because `mc` is a Go binary optimized for Go's HTTP stack.
This is a client-side limitation, not a server-side one.

The L4 PUT gap (3.7x vs disk) is from inline hashing (MD5 at 703 MB/s is the
floor) plus RS encode plus metadata writes. These are fundamental costs of
erasure-coded storage, not inefficiencies.

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

### Summary

| Path | Data-path allocs/GB | Remaining alloc | Can eliminate? |
|------|---------------------|-----------------|----------------|
| **1+0 GET** | **0** | -- | done |
| **EC GET (healthy)** | **0 (steady state)** | 8 warmup allocs, then pool recycles | **done** -- BufPool + PooledBuf + Bytes::from_owner |
| **EC GET (degraded)** | ~5400 | Vec per shard for RS reconstruct | no: RS API requires owned buffers |
| **PUT (per-block loop)** | **0** | -- | done |
| **PUT (total request)** | ~10 | setup + finalize allocs | no: inherent (metadata, hashers, writers) |
| **Response wrapper** | **0** | -- | done |

## Log-structured storage (implemented)

Small objects (<= 64KB) now use a log-structured storage path instead of
the file-per-object layout. See [write-log.md](write-log.md) for full design.

| | File tier | Log store | Improvement |
|--|----------|-----------|-------------|
| **4KB PUT total** | 2.0ms | **1.2ms** | **40% faster** |
| **4KB GET total** | 1.7ms | **1.0ms** | **41% faster** |
| **4KB PUT server only** | 1.33ms | **0.53ms** | **60% faster** |
| **4KB GET server only** | 0.95ms | **0.31ms** | **67% faster** |
| Write ops per 4KB (4 disks) | 12 | **4** | 3x fewer |
| Files per 1M small objects | 3M+ | ~3 segments | ~1000x fewer |
| Activation | default | opt-in: `mkdir .abixio.sys/log/` | |

"Server only" = total minus TCP connect. Windows TCP localhost = 0.68ms
(57% of total). Linux TCP localhost = ~0.03ms. With keep-alive, connect
cost is zero after first request.

No fsync on writes. Page cache serves both read (mmap) and write
(file.write_all) paths. Same durability model as MinIO/RustFS.

Active segment is mmap'd at creation -- reads see writes immediately via
shared page cache. No sealing needed for reads. One active segment per disk.

**Windows `localhost` warning**: never use `localhost` for benchmarks.
Windows DNS resolution adds ~200ms per request. Always use `127.0.0.1`.

Remaining: GC, heal log-awareness, versioned objects.

## Future optimization candidates

- **Multi-disk benchmark on separate physical devices**: parallel shard writes
  would likely help when shards map to different NVMe drives
- **io_uring on Linux**: zero-copy I/O could narrow the L4-vs-L3 gap
- **Configurable fsync**: per-bucket durability modes (none/data/full)
- **s3s upgrade or bypass**: s3s 0.13 adds ~38% overhead on PUT. Newer versions
  or a custom protocol layer could reduce this
- **Streaming multipart GET**: currently falls back to buffered path
- **Streaming range requests**: currently falls back to buffered path
- **Log store GC**: reclaim dead space from sealed segments
- **Log store batch fsync**: configurable fsync interval for higher write throughput
