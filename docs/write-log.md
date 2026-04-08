# Log-structured storage

Datrium DiESL-inspired log-structured storage for small objects. Objects are
written exactly once to an append-only log, never overwritten. GC reclaims
dead space. The log IS the permanent storage -- no flush, no second format.

Large objects (>64KB) keep the existing file-per-object layout.

## Measured performance

| | File tier | Log store | Change |
|--|----------|-----------|--------|
| 4KB PUT | 2.5ms | **1.5ms** | **40% faster** |
| 4KB GET | 1.9ms | **1.2ms** | **37% faster** |
| Filesystem ops per 4KB (4 disks) | 12 | **4** | 3x fewer |
| Files per 1M small objects | 3M+ | ~3 segments | ~1000x fewer |

No fsync on writes -- trust OS page cache, same as MinIO and RustFS.
Writes go to page cache (RAM), reads from the same pages via mmap.
Both sides hit RAM. Disk flush happens when the OS decides.

## How it works

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

We measured. The mmap read itself takes ~100-200 nanoseconds (page cache hit).
The 1.2ms GET latency is dominated by HTTP overhead:

```
~0.5ms  TCP + HTTP parse (hyper)
~0.3ms  s3s request dispatch
~0.1ms  bucket validation + versioning cache
~0.1ms  log store mutex + index lookup
~0.001ms  mmap read (the actual data)
~0.2ms  response serialization + HTTP write
= ~1.2ms total
```

A userspace HashMap cache would save 0.001ms off a 1.2ms request. The
bottleneck is the HTTP protocol stack, not the storage read. To get
microsecond latency, you'd need to bypass HTTP entirely.

## Architecture

### Active segment

One active segment per disk. Pre-allocated 64MB. Receives all appends.
**Also serves reads** via mmap of the same file (page-cache coherent with
writes). No sealing needed for reads -- the mmap sees writes immediately.

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

This matches MinIO and RustFS behavior -- neither fsyncs individual writes.
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
