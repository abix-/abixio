# Layer optimization

Systematic per-layer performance analysis of the AbixIO PUT and GET paths.
Each layer is treated as an independent optimization problem: measure it,
identify the ceiling, find the bottleneck, test a fix, record the result.

Last updated: 2026-04-07, commit `2688069`.

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
GET 10MB (4d EC)   774 MB/s        675 MB/s        mmap+RS (4MB blocks)
GET 1GB (4d EC)    919 MB/s        803 MB/s        mmap+RS (4MB blocks)
PUT (all sizes)    unchanged       unchanged       0%
```

Note: EC GET is slightly slower because mmap slices are copied to Vec for
RS decode. The 1+0 fast path (no EC) is where the huge win is.

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
- EC GET (4 disk): 675-803 MB/s (slightly lower due to mmap slice->Vec copy
  for RS decode, but fewer iterations from 4MB blocks)

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
| 5 | mmap GET (EC, 4MB blocks) | L4 | 4-disk GET ~800 MB/s (4MB blocks, fewer iterations) | done |

## Remaining gaps

| Gap | Current | Ceiling | Ratio | Notes |
|-----|---------|---------|-------|-------|
| L6 PUT vs L4 PUT | 272 vs 439 MB/s | 1.6x | s3s dispatch overhead |
| L4 PUT vs L3 write | 439 vs 1625 MB/s | 3.7x | hashing + RS + metadata |
| L6 GET (mc) vs L6 GET (curl) | 354 vs 1220 MB/s | 3.4x | mc client overhead (SigV4, Go HTTP) |
| EC GET vs 1+0 GET | 803 vs page cache | -- | RS decode + mmap->Vec copy |

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

### EC GET hot path (`src/storage/erasure_decode.rs` read_and_decode_stream)

Per 1GB object with 4-disk 3+1 EC (1024 encode blocks, 256 decode iterations):

| Location | What | Count per 1GB | Impact | Fix |
|----------|------|---------------|--------|-----|
| line 158 | `Vec::with_capacity(iter_size)` -- 4MB output buffer | 256 | **high** | pre-alloc once, `clear()` + reuse. `Bytes::from(take(&mut buf))` per send |
| line 166 | `vec![None; total]` -- shard slot array | 1024 | medium | pre-alloc once outside loop, reuse |
| line 171 | `mmap[..].to_vec()` -- **copies mmap data into Vec per shard per block** | 1024 x 4 = 4096 | **critical** | fast path: when all data shards present, slice directly from mmap into output, skip Vec entirely. slow path: only alloc Vec when RS reconstruct needed |
| line 191 | `Vec::with_capacity(block_size)` -- per-block reassembly buffer | 1024 | **high** | eliminate: write directly into iter_data in the fast path |

**Total per 1GB**: ~5400 allocations. The mmap->Vec copy at line 171 is
the single biggest offender -- it copies every byte of every shard even
though we already have the data memory-mapped.

**Fast path** (all shards healthy, common case): zero Vec allocations.
Read data shards directly from mmap slices into the output buffer.

**Slow path** (missing shards, needs RS reconstruct): allocate Vecs for
shard slots, call `rs.reconstruct()`, then copy to output. This is rare
and correctness matters more than speed.

### PUT hot path (`src/storage/erasure_encode.rs` encode_and_write)

Per 1GB object with 3+1 EC (1024 blocks):

| Location | What | Count per 1GB | Impact | Fix |
|----------|------|---------------|--------|-----|
| line 126 | `Vec::with_capacity(STREAM_BLOCK_SIZE * 2)` block_buf | 1 | none | already reused across blocks |
| line 34-41 | `split_data()` -- creates `data_n` new Vecs per block | 1024 x 3 = 3072 | **critical** | `split_data_into()`: pre-alloc shard buffers, write into them, reuse |
| line 283-285 | parity Vec allocation per block | 1024 x 1 = 1024 | **high** | pre-alloc parity buffers, zero and reuse |
| line 290-292 | `shard_hashers[i].update()` + `writers[i].write_chunk()` | per shard per block | none | no allocation (update is in-place, write is I/O) |

**Total per 1GB**: ~4096 allocations from split_data + parity buffers.

**Fix**: `split_data_into(data, count, &mut buffers)` writes into pre-allocated
buffers. Parity buffers also pre-allocated. Both reused across all 1024 blocks.

### Summary

| Path | Allocations per 1GB (before) | Allocations per 1GB (after) | Reduction |
|------|-----------------------------|-----------------------------|-----------|
| 1+0 GET | 0 (mmap -> Bytes slices) | 0 | already done |
| EC GET (healthy) | ~5400 | ~256 (one per 4MB send) | 95% |
| EC GET (degraded) | ~5400 | ~5400 (reconstruct needs Vecs) | 0% (correctness path) |
| PUT | ~4096 | ~1 (initial alloc, reused) | 99% |

## Future optimization candidates

- **Multi-disk benchmark on separate physical devices**: parallel shard writes
  would likely help when shards map to different NVMe drives
- **io_uring on Linux**: zero-copy I/O could narrow the L4-vs-L3 gap
- **Configurable fsync**: per-bucket durability modes (none/data/full)
- **s3s upgrade or bypass**: s3s 0.13 adds ~38% overhead on PUT. Newer versions
  or a custom protocol layer could reduce this
- **Streaming multipart GET**: currently falls back to buffered path
- **Streaming range requests**: currently falls back to buffered path
