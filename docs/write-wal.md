# Write-ahead log (WAL)

**Authoritative for:** how the WAL write path works, what happens
before and after ack, materialization, and recovery.

## What it is

The WAL is a fast write path for small objects. There is always an
active segment open and ready for writes. PUTs append to this segment
via mmap (zero syscalls) and ack immediately. A background worker
materializes each entry to its final file-per-object location
(shard.dat + meta.json). Once materialized, the WAL entry is removed
from the pending map. When the active segment fills up, it is sealed
and a new active segment is created. Sealed segments are deleted
after all their entries have been materialized.

The WAL is ephemeral. It is not the permanent storage. The final
resting place is always the file tier. But the active segment is
always open and always fast.

## How it works

### Write path (PUT, object <= wal_threshold)

```
1. Needle::new()              serialize ObjectMeta to msgpack
2. wal.lock()                 acquire Mutex 
3. serialize_into(mmap)       write needle directly into mmap (zero alloc, zero syscalls)
4. pending.insert()           HashMap insert so reads can find it
5. try_send(req)              fire-and-forget to materialize worker
                              (Arc<str> bucket/key + 20 bytes of offsets, no data copy)
6. return Ok                  ack to caller
```

Total hot path at 4KB: ~147us (vs 143us log, 159us pool, 793us file).
Head-to-head unit test: 25us WAL vs 30us log (WAL is fastest).

### Write path (PUT, object > wal_threshold)

Straight to file tier. No WAL involvement. The write amplification of
appending to a segment then writing to final files is not worth it when
raw I/O dominates at large sizes.

Default threshold: 64KB (same as the old LOG_THRESHOLD). Configurable
via `--wal-threshold`.

### Read path (GET)

```
1. check WAL pending map     HashMap lookup by (bucket, key)
   FOUND  -> read data from segment mmap (zero-copy slice -> Vec)
   FOUND  -> read meta from segment mmap, decode msgpack
   return (data, meta)

2. NOT FOUND -> fall through to file tier
   open shard.dat, read meta.json, return
```

Objects in the WAL pending map are served from the segment's mmap.
Objects that have been materialized are served from the file tier.
The transition is seamless -- once materialized, the pending entry
is removed, and subsequent reads go to the file tier.

### Background materialize worker

```
1. receive MaterializeRequest from channel
   (contains only bucket, key, and segment offsets -- no data)
2. lock WAL, read data + meta from segment mmap
3. decode meta, build ObjectMetaFile, serialize to JSON (simd-json)
4. create_dir_all for object directory
5. write shard.dat + meta.json concurrently (tokio::try_join!)
6. lock WAL, call mark_materialized(bucket, key)
   -> removes entry from pending map
   -> decrements segment pending count
   -> if segment fully drained, deletes segment file
```

The worker does all heavy lifting: mmap reads, JSON serialization,
directory creation, file writes. None of this runs on the PUT hot path.

Multiple workers run in parallel via round-robin dispatch (same pattern
as the pool's rename workers). Default: 2 workers.

### Shutdown

On ctrl+c, the materialize workers drain all remaining channel items
before exiting. Every acked PUT reaches its final file-per-object
location. Same pattern as the pool's rename worker shutdown.

### Crash recovery

On startup, `Wal::open()`:

1. Scans the WAL directory for existing segment files
2. For each needle in each segment:
   - Check if shard.dat exists at the final destination
   - If yes: already materialized, skip
   - If no: add to pending map, send MaterializeRequest to worker
3. Delete fully-drained segments (all entries materialized)
4. Create a fresh active segment for new writes

Recovery cost is bounded by the number of un-materialized entries,
not total data size. Under normal operation, the materialize worker
keeps up, so recovery processes near-zero entries.

## Optimizations applied

Three optimizations brought the WAL from 197us to 147us at 4KB:

### 1. MmapMut writes (segment.rs)

Replaced `file.seek() + file.write_all()` (two syscalls per append)
with `mmap[offset..].copy_from_slice()` (zero syscalls, memcpy only).
The pre-allocated segment file is mmap'd as writable (`MmapMut`).
Reads and writes use the same mmap -- no coherence issues.

Impact: ~10-15us saved per append on Windows.

### 2. Zero-allocation serialize_into (needle.rs)

Old path: `Needle::serialize()` allocated two Vecs (payload for
checksum + output buffer), then `append` copied the Vec into mmap.
Three copies of the data.

New path: `Needle::serialize_into(&mut mmap[offset..])` writes header,
bucket, key, meta, and data directly into the mmap region. Checksum
is computed over the payload bytes already in the mmap. One copy of
the data, zero allocations.

Impact: ~1-2us at 4KB, ~5-10us at 64KB (proportional to data size).

### 3. Fire-and-forget channel send (local_volume.rs)

Replaced `channel.send(req).await` with `channel.try_send(req)`.
The data is durable in the WAL segment after `wal.append()`. The
materialize request is best-effort -- if the channel is full, recovery
picks it up on restart. `try_send` avoids the `.await` entirely.

Impact: removes async yield point from hot path.

### 4. Arc<str> identity (wal.rs, local_volume.rs)

`MaterializeRequest` uses `Arc<str>` for bucket and key instead of
`String`. The WAL pending map already uses `Arc<str>`, so the request
clones the reference count (~5ns) instead of heap-allocating (~200ns).

Impact: ~400ns saved per PUT.

## Design decisions

**WAL is ephemeral, not permanent.** The WAL is a landing zone.
Objects leave as soon as they are materialized. No compaction needed.

**Worker reads from mmap, not from request.** The MaterializeRequest
carries only offsets (20 bytes), not data copies. The worker reads
data from the segment mmap when it is ready to write. This keeps the
PUT hot path allocation-free.

**Size threshold.** Objects above 64KB go straight to file tier. The
write amplification of WAL (write to segment + write to final files)
is only worth it when filesystem metadata overhead is a significant
fraction of total write time. At large sizes, raw I/O dominates.

## Three-tier architecture

The WAL is part of a three-tier storage architecture:

| Tier | Owns | Status |
|---|---|---|
| **WAL** | fast writes (append to mmap, materialize in background) | implemented |
| **Read cache** | fast reads for hot small objects (LRU/frequency, bounded RAM) | planned |
| **File tier** | permanent storage (shard.dat + meta.json, inspectable) | implemented |

Each tier is independent and does one job:

- WAL does not try to serve reads long-term (that's the read cache's job)
- Read cache does not try to write durably (that's the WAL's job)
- File tier does not try to be fast (that's the WAL and read cache's job)

Each tier does one job. Splitting write, read, and storage
responsibilities into three tiers keeps each one simple.

