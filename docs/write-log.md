# Log-structured storage

Log-structured storage for small objects. Objects are written exactly
once to an append-only log, never overwritten. GC reclaims dead space.
The log IS the permanent storage. No flush, no second format.

Large objects (>64KB) keep the existing file-per-object layout.

## Performance

End-to-end log-store PUT and GET at 4KB through 100MB live in
[write-path.md::Branch B](write-path.md#branch-b-local-log-store).
Per-tier comparison vs the file and pool tiers (the "log store vs file
tier" view) lives in
[write-path.md::Where the time goes](write-path.md#where-the-time-goes).
Cross-server competitive numbers (AbixIO vs MinIO vs RustFS) live in
[benchmarks.md::Comprehensive matrix](benchmarks.md#comprehensive-matrix).
None of those tables are duplicated here.

The two pieces of perf information that *are* unique to this doc, and
therefore live here:

### Filesystem-shape advantage

| | File tier | Log store | Ratio |
|---|---|---|---|
| Filesystem ops per 4KB PUT (4 disks) | 12 | **4** | 3x fewer |
| Files per 1M small objects             | 3M+ | ~3 segments | ~1000x fewer |

This is a structural advantage, not a runtime measurement. It's the
reason small-object PUT through the log store does so much less work
at the storage layer.

### Durability model

No fsync on writes. Trust OS page cache, same as MinIO and RustFS.
Writes go to page cache (RAM), reads from the same pages via mmap.
Both sides hit RAM. Disk flush happens when the OS decides. The full
durability story is in the [Durability model](#durability-model)
section below.

## How it works

For the canonical end-to-end PUT path and branch selection, see
[write-path.md](write-path.md). This page stays focused on the log tier
itself.

```
PUT 4KB object (Content-Length <= 64KB):
  -> RS encode into shards
  -> serialize needle (header + msgpack meta + shard data)
  -> file.write_all(needle) to active segment   <- goes to page cache (RAM)
  -> update in-memory index (HashMap)
  -> ack to client
  Total: one file write per disk. No mkdir, no file create, no fsync.

GET 4KB object:
  -> HashMap lookup: bucket+key -> segment:offset
  -> mmap[offset..len]                           <- reads from page cache (RAM)
  -> return shard data
  Total: one HashMap lookup + one pointer dereference. No file open.
```

## Why not a userspace RAM cache on top?

We measured. The total 1.0ms GET breaks down as:

```
4KB GET latency breakdown:
  TCP 3-way handshake:   0.68ms   <- 68% of total (Windows; ~0.03ms on Linux)
  server processing:     0.31ms   <- the actual work
    hyper HTTP parse:      ~0.08ms
    s3s dispatch:          ~0.10ms
    log store lookup:      ~0.05ms  (mutex + HashMap)
    mmap read:             ~0.0002ms (200 nanoseconds)
    s3s response:          ~0.08ms
  total:                 1.00ms
```

The mmap read is 200 nanoseconds. A userspace RAM cache would save
~0.05ms (the mutex + HashMap lookup). The bottleneck is TCP connect
(0.68ms) and HTTP protocol overhead (0.26ms), not storage.

With HTTP keep-alive (persistent connections), TCP connect drops to zero
after the first request. Server-only processing of 0.31ms = ~3200 GET/sec
per connection. On Linux (0.03ms connect), total drops to ~0.34ms.

## Architecture

### Active segment

One active segment per disk. Pre-allocated 64MB. Receives all appends.
**Also serves reads** via mmap of the same file (page-cache coherent with
writes). No sealing needed for reads; the mmap sees writes immediately.

### Sealed segments

When the active segment fills (64MB), it becomes sealed (read-only, mmap'd).
A new active segment is created. Sealed segments are opened on startup for
crash recovery.

### Needle format

```
[24 bytes header]
  magic: u32              = 0xAB1C_4E00
  flags: u8               = 0 normal | 1 delete
  bucket_len: u8
  key_len: u16
  meta_len: u16           (msgpack ObjectMeta)
  data_len: u32           (shard data)
  checksum: u64           (xxhash64 of everything after header)
  _pad: u16

[bucket: bucket_len bytes]
[key: key_len bytes]
[meta: meta_len bytes]    (msgpack, ~200 bytes)
[data: data_len bytes]    (shard data)
```

Self-describing. Scannable. Each needle checksummed independently.

### In-memory index

HashMap: (bucket, key) -> (segment_id, offset, lengths).
~32 bytes per entry + key strings. 1M objects = ~60MB RAM.
Rebuilt from segments on startup by sequential scan (~1 sec/GB).

### On-disk layout

```
volume_root/
  .abixio.sys/
    volume.json                (existing)
    log/
      segment-000001.dat       (sealed, mmap'd)
      segment-000002.dat       (sealed, mmap'd)
      segment-000003.dat       (active, appending + mmap'd)
  bucket/
    large-key/
      shard.dat                (file tier for >64KB)
      meta.json
```

### Durability model

Writes go to OS page cache via `file.write_all()`. No fsync per write.
The OS flushes dirty pages to disk on its own schedule (~30 seconds on
Linux, similar on Windows). Process crashes don't lose data (page cache
is in kernel memory). Power loss loses unflushed writes (UPS mitigates).

This matches MinIO and RustFS behavior. Neither fsyncs individual writes.
Confirmed in RustFS source: sync flag is a TODO that does nothing.

## How to enable

```bash
mkdir -p /path/to/volume/.abixio.sys/log
```

The log store activates automatically when this directory exists.

## Status

**Implemented and wired into S3 PUT/GET path.**

Working:
- Small PUT (Content-Length <= 64KB): needle append to log segment
- Small GET: in-memory index lookup + mmap read from segment
- Small DELETE: tombstone needle + index removal
- Crash recovery: segment scan rebuilds index on startup
- Active segment mmap: reads work without sealing

Remaining:
- GC: reclaim dead space from sealed segments
- Heal worker: read shards from log (currently file-tier only)
- Versioned objects: currently bypass the log
- Admin inspect: show segment:offset for log-stored objects

## Implementation

| File | What |
|------|------|
| `src/storage/needle.rs` | Needle format: 24-byte header, msgpack meta, xxhash64 checksum |
| `src/storage/segment.rs` | Segment files: pre-alloc 64MB, append, mmap (active + sealed), scan |
| `src/storage/log_store.rs` | LogStore: in-memory index, segment lifecycle, crash recovery |
| `src/storage/local_volume.rs` | Integration: write_shard/read_shard/stat_object/mmap_shard route through log |
| `src/storage/volume_pool.rs` | S3 routing: small PUTs collected and encoded via write_shard path |
| `src/s3_service.rs` | Passes content_length to put_object_stream for size-based routing |

## See also

- [Pre-opened temp file pool](write-pool.md): a replacement for the
  log store with benchmark results now documented. Same
  eliminate-syscalls-from-the-hot-path goal, different mechanism (one
  pre-opened file per object instead of many objects per segment).
  The pool's main draw is that it has no GC: one file per object
  means `unlink()` reclaims space natively, no compactor needed. The
  newer benchmark conclusion is a three-tier split: log store for
  `<=64KB`, pool for mid-range writes, file tier for large objects.
- [RAM write cache](write-cache.md): orthogonal, writes to a DashMap
  in RAM with peer replication, flushes to whichever lower tier is
  active.

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| Small objects `<=64KB` route through the log store when enabled | Verified | `src/storage/volume_pool.rs:239-365`, `src/storage/local_volume.rs:564-568` |
| Versioned objects bypass the log store | Verified | `src/storage/local_volume.rs:564-567` |
| Log store uses append-only segments plus an in-memory index | Verified | `src/storage/log_store.rs:18-26`, `121-156`, `183-192` |
| Segment files are pre-allocated to 64MB | Verified | `src/storage/segment.rs:19-20`, `54-75` |
| Crash recovery rebuilds the index by scanning segments on startup | Verified | `src/storage/log_store.rs:29-75`, `78-103` |
| Tombstone/delete support exists for small deletes | Verified | `src/storage/log_store.rs:158-172` |
| Log-store reads are zero-copy in all described paths | Needs nuance | The implementation has mmap-backed segments, but `LogStore::read_data_vec` still returns copied `Vec` data in the path described here (`src/storage/log_store.rs:195-210`) |
| `abixio-ui/src/bench/` is the source of the published 4KB performance numbers | Plausible but not re-run in this pass | The benchmark script exists and is referenced from docs/tests, but the numbers were not reproduced during this audit |
| Pool is “currently being benchmarked” to decide the default | Corrected | Newer docs already conclude the benchmark outcome in `docs/benchmarks.md` and `docs/write-pool.md` |
| `--write-tier` currently gates the decision | Not code-verified in this pass | Other docs say the CLI flag is still pending; I did not verify `src/main.rs` wiring here |

Verdict: the implementation description is broadly accurate for the log-store design and routing, but some prose overstates the zero-copy behavior of the simple read path. The benchmark tables remain benchmark-derived claims, not code-derived facts.
