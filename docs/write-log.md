# Write log design

Datrium-style write coalescing for small objects. Design doc only -- not
implemented yet.

## Problem

A 4KB PUT on AbixIO with 3+1 EC creates per disk:
- 1 `mkdir -p` (directory tree for key path)
- 1 file create + write (`shard.dat`, ~1.3KB per shard)
- 1 file create + write (`meta.json`, ~500 bytes)

Total: **12 filesystem operations across 4 disks for 4KB of user data.**

On NTFS, each file create involves MFT entry allocation, directory entry
update, and journal write. At small-object scale (thousands of PUTs/sec),
this becomes the bottleneck -- not network, not CPU, not hashing.

The L4 benchmark shows 4KB PUT at 2-4 MB/s. That's ~500-1000 objects/sec,
entirely limited by filesystem metadata overhead.

## What Datrium does

1. **Buffer writes in NVRAM** -- ack immediately, durable in mirrored NVRAM
2. **Collect blocks into objects** -- many small writes coalesce into ~10MB
3. **Append to log** -- write the coalesced object sequentially, never overwrite
4. **Background compaction** -- garbage collect deleted/overwritten data

Result: thousands of 4KB random writes become one 10MB sequential write.

## What AbixIO would do

### Write path (hot)

```
PUT 4KB object
  -> RS encode (split into shards, ~1.3KB each)
  -> compute blake3 + MD5 inline
  -> append shard data + metadata to write log (one file per disk)
  -> ack to client

write log entry format (per shard):
  [header: 32 bytes]
    magic: u32          (0xAB1X)
    version: u8         (1)
    flags: u8           (0 = normal, 1 = delete marker)
    bucket_len: u16
    key_len: u16
    meta_len: u32       (serialized ObjectMeta)
    data_len: u32       (shard data)
    checksum: u64       (xxhash of entire entry for crash recovery)
  [bucket: bucket_len bytes]
  [key: key_len bytes]
  [meta: meta_len bytes]  (JSON ObjectMeta, same as current meta.json)
  [data: data_len bytes]  (shard data, same as current shard.dat)
```

The write log is an append-only file. One per disk volume. Entries are
variable-length, self-describing, and checksummed.

For a 4KB object with 3+1 EC:
- Shard size: ~1.3KB
- Entry size: 32 + ~10 + ~30 + ~500 + ~1300 = ~1.9KB per shard
- Total: 4 x 1.9KB = **7.6KB across 4 disks, 4 append writes**

vs current: 12 filesystem operations (4 mkdirs + 4 file creates + 4 meta creates)

### Flush (background)

When the write log reaches a threshold (e.g. 10MB or 1000 entries):

```
flush task (per disk):
  read log entries since last flush
  group by bucket/key
  for each object:
    write shard.dat to final location (same layout as today)
    write meta.json to final location
  update flush checkpoint (log offset)
  fsync
```

After flush, the objects are in the same on-disk format as today. Existing
GET/HEAD/LIST paths work unchanged. The write log is a write-ahead buffer,
not a replacement for the storage layout.

### Read path (unchanged for flushed objects)

Objects that have been flushed are read via mmap exactly as today. Zero-copy
GET works unchanged.

### Read path (for unflushed objects)

Objects still in the write log need a lookup:

```
GET for key "photos/cat.jpg":
  1. check in-memory index (hash map: bucket+key -> log offset)
  2. if found: read from write log at offset (mmap the log file)
  3. if not found: read from flushed location (existing path)
```

The in-memory index is rebuilt from the write log on startup (scan entries,
keep latest per key). This is the same recovery pattern as any WAL.

### Delete path

A delete appends a tombstone entry to the write log (flags = delete marker).
The in-memory index updates. Compaction removes the data later.

### Compaction (background)

Compaction reclaims space from overwritten/deleted objects:

```
compaction task:
  scan write log from oldest unflushed offset
  skip entries where a newer entry exists for the same key
  skip tombstoned entries
  rewrite remaining live entries to a new log segment
  delete old segment
```

This is the same as LSM compaction. One level is enough for an object store
(objects are immutable, no merge needed).

## On-disk layout

```
volume_root/
  .abixio.sys/
    volume.json           (existing)
    write-log/
      segment-000001.log  (append-only log segment)
      segment-000002.log
      checkpoint.json     (last flushed offset per segment)
  bucket/
    key/
      shard.dat           (flushed shard data, same as today)
      meta.json           (flushed metadata, same as today)
```

Log segments are fixed-size (e.g. 64MB). When a segment fills, a new one
is created. Old segments are deleted after compaction.

## Crash recovery

On startup:

1. Read `checkpoint.json` -- last known flushed offset per segment
2. Scan each segment from the checkpoint offset forward
3. Validate each entry's checksum (xxhash)
4. Entries with valid checksums: add to in-memory index
5. Entries with invalid checksums: discard (partial write from crash)
6. Resume normal operation

Correctness: the write log is append-only with per-entry checksums. A crash
mid-write truncates the last entry, which the checksum detects. All prior
entries are intact. The client received an ack only after the append + fsync
completed, so any acked write is in the log.

## Size-adaptive routing

Not all objects benefit from the write log. Large objects (>1MB) should
bypass the log and write directly to shard files (current path):

```
PUT request
  -> if object_size <= WRITE_LOG_THRESHOLD (e.g. 64KB):
       append to write log (fast path)
  -> else:
       write shard files directly (streaming path, current code)
```

The threshold is tunable. The default (64KB) means:
- Small metadata, thumbnails, configs -> write log (coalesced)
- Photos, videos, backups -> direct write (streaming, no double-write)

## What changes in the codebase

| Component | Change |
|-----------|--------|
| `Backend` trait | add `append_to_log()` method |
| `LocalVolume` | implement write log (append, flush, compact, recover) |
| `RemoteVolume` | no change (remote volumes don't coalesce locally) |
| `VolumePool` | route small objects to write log, large to direct |
| `encode_and_write` | detect size, call log or direct path |
| GET path | check in-memory index before disk lookup |
| startup | scan write logs, rebuild index, flush if needed |

## What does NOT change

- On-disk format for flushed objects (shard.dat + meta.json)
- GET/HEAD/LIST for flushed objects
- Streaming PUT for large objects
- RS encode/decode
- Cluster protocol
- S3 API behavior

## Expected impact

| Metric | Current (4KB PUT) | With write log |
|--------|-------------------|----------------|
| Filesystem ops per object | 12 (4 disks) | 4 (one append per disk) |
| IOPS ceiling | ~500-1000/sec | ~10,000+/sec (sequential appends) |
| Write amplification | 1x (but many small files) | <1x (coalesced, fewer metadata ops) |
| GET latency (unflushed) | n/a | +1 hash lookup (in-memory index) |
| GET latency (flushed) | unchanged | unchanged |
| Crash recovery | instant (files are files) | scan write log (fast, ~100MB/sec) |

## Trade-offs

| Pro | Con |
|-----|-----|
| 10x+ small-object IOPS | Added complexity (WAL + index + compaction) |
| Reduced NTFS metadata overhead | Double-write for small objects (log + flush) |
| Sequential I/O on all disks | Memory for in-memory index (~100 bytes/object) |
| Better SSD longevity | Recovery time proportional to unflushed log size |
| Background flush is tunable | Flush lag = data in log but not in final location |

## Open questions

1. **Fsync strategy**: fsync per entry (safest, slowest), per batch (good
   trade-off), or per flush (fastest, data at risk between flushes)?
2. **Index persistence**: rebuild from log on startup (simple) or persist
   the index separately (faster recovery)?
3. **Multi-object transactions**: should the write log support atomic
   multi-key writes? (needed for multipart complete)
4. **Versioning interaction**: versioned objects append new versions to the
   log -- how does the in-memory index track version chains?
5. **Compaction scheduling**: time-based, size-based, or idle-triggered?
