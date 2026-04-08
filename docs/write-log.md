# Log-structured storage

Datrium-style log-structured storage for small objects. Objects are written
exactly once to an append-only log, never overwritten. GC reclaims dead space.
The log IS the permanent storage -- no flush, no second format, no double-write.

Large objects (>64KB) keep the existing file-per-object layout (shard.dat +
meta.json). The log is only for small objects where filesystem metadata
overhead (mkdir, file create) dominates.

## Problem

A 4KB PUT on AbixIO with 3+1 EC creates per disk:
- 1 `mkdir -p` (directory tree for key path)
- 1 file create + write (`shard.dat`, ~1.3KB per shard)
- 1 file create + write (`meta.json`, ~500 bytes)

**12 filesystem operations across 4 disks for 4KB of user data.**
L4 benchmark: 4KB PUT at 2-4 MB/s (~500-1000 objects/sec).

## Solution

Append shard data + metadata as a single needle to an append-only log
segment file. One write per disk instead of three filesystem operations.

```
PUT 4KB object
  -> RS encode into shards
  -> append each shard as a needle to the disk's log segment
  -> update in-memory index
  -> fsync
  -> ack

GET 4KB object
  -> lookup in-memory index -> segment:offset
  -> mmap slice from segment (zero-copy)
  -> return
```

## On-disk layout

```
volume_root/
  .abixio.sys/
    volume.json                 (existing, unchanged)
    log/
      segment-000001.dat        (sealed, mmap'd for GET)
      segment-000002.dat        (sealed)
      segment-000003.dat        (active, receiving appends)
  bucket/
    large-key/
      shard.dat                 (file tier for >64KB, unchanged)
      meta.json
```

## Needle format

Each needle is a self-describing, checksummed record containing one shard
of one object (metadata + data together).

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
[meta: meta_len bytes]    (msgpack, ~200 bytes typical)
[data: data_len bytes]    (shard data)
```

4KB object needle: ~1.6KB per shard. One append per disk.

## Segment lifecycle

Segments are pre-allocated 64MB files. One active segment per disk at a time.

```
NEW -> ACTIVE (appending) -> SEALED (full, read-only, mmap'd) -> GC -> DEAD
```

## In-memory index

HashMap: (bucket, key) -> (segment_id, offset, lengths). Rebuilt from
segments on startup. ~60MB RAM per 1M objects.

## Garbage collection

When a sealed segment has >50% dead space: scan for live needles, copy them
to the active segment, delete old segment. No compaction levels, no sorting.

## Crash recovery

Scan segments sequentially, validate each needle's xxhash64 checksum.
Valid = add to index. Invalid/truncated = stop (partial write). ~1 sec/GB.

## Expected impact

| Metric | Current | Log-structured |
|--------|---------|---------------|
| 4KB PUT (4 disks) | 12 fs ops, ~2 MB/s | 4 appends, ~20+ MB/s |
| 4KB GET (1 disk) | file open+read, ~8 MB/s | mmap slice, page cache |
| Files per 1M objects | 3M+ | ~16 segments |

## Status

**Implemented and wired into S3 path.** Small PUTs with Content-Length <= 64KB
route through the log store end-to-end. GETs check the log store first via
mmap, fall through to file tier. Verified with curl and all 320 tests pass.

Remaining work:
- GC: reclaim dead space from sealed segments (phase 8)
- Heal worker: read shards from log, not just files (phase 7)
- Versioned objects: currently bypass the log (phase 9)
- Chunked-transfer PUTs: no Content-Length, bypass the log (acceptable)

## How to enable

The log store activates when `.abixio.sys/log/` exists on a volume:

```bash
# enable on an existing volume
mkdir -p /path/to/volume/.abixio.sys/log

# or programmatically
volume.enable_log_store()
```

On restart, the log store scans existing segments and rebuilds the in-memory
index automatically.

## Implementation

| File | What |
|------|------|
| `src/storage/needle.rs` | Needle format: 24-byte header, msgpack meta, xxhash64 checksum |
| `src/storage/segment.rs` | Segment files: pre-alloc 64MB, append, seal, mmap, scan |
| `src/storage/log_store.rs` | LogStore: in-memory index, segment lifecycle, crash recovery |
| `src/storage/local_volume.rs` | Integration: write_shard/read_shard/stat_object/mmap_shard route through log |
| `src/storage/volume_pool.rs` | S3 routing: small PUTs collected and encoded via write_shard path |
| `src/s3_service.rs` | Passes content_length to put_object_stream for size-based routing |
