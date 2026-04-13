# Write-ahead log (WAL)

**Authoritative for:** how the WAL write path works, what happens
before and after ack, materialization, recovery, and how it replaces
the log store and write pool.

## What it is

The WAL is a fast write path for small objects. PUTs append to an
already-open segment file and ack immediately. A background worker
materializes each entry to its final file-per-object location
(shard.dat + meta.json). Once materialized, the WAL entry is deleted.
When all entries in a segment are materialized, the segment file is
deleted.

The WAL is ephemeral. It is not the permanent storage. The final
resting place is always the file tier.

## Why it exists

The file tier pays mkdir + File::create + write + close on every PUT.
At 4KB, that filesystem metadata overhead is ~800us -- 80% of the total
write time. The WAL eliminates it by appending to an already-open file.

The log store solved the same problem but made the log permanent,
which required GC (compaction of dead needles), a full in-memory index
of every object, and a startup rebuild that scales with total data size.

The write pool solved it with pre-opened temp files, but required
slot management, replenishment, and a DashMap of pending renames.

The WAL replaces both with one mechanism: fast append, background
materialize, delete when done.

## How it works

### Write path (PUT, object <= wal_threshold)

```
1. Needle::new()          serialize ObjectMeta to msgpack, build needle buffer
2. wal.lock()             acquire Mutex (same as log store)
3. segment.append()       write needle bytes to already-open segment file
4. pending.insert()       HashMap insert so reads can find it
5. channel.send()         send lightweight request to materialize worker
                          (Arc<str> bucket/key + 20 bytes of offsets, no data copy)
6. return Ok              ack to caller
```

Total hot path at 4KB: ~155us (vs 135us log, 157us pool, 798us file).

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

## What it replaces

| Old path | What it did | Problem | WAL equivalent |
|---|---|---|---|
| log store | append needle to permanent log, serve from log forever | GC (dead needles), RAM index scales with total data, startup rebuild reads all data | append to WAL segment, materialize to files, delete segment |
| write pool | write to pre-opened temp files, background rename | slot management, DashMap of pending renames, replenishment | append to WAL segment, background materialize |

## Architecture

### Files

- `src/storage/wal.rs` -- Wal struct, WalEntry, MaterializeRequest, materialize worker, recovery
- `src/storage/segment.rs` -- ActiveSegment (reused from log store, unchanged)
- `src/storage/needle.rs` -- Needle serialization (reused from log store, unchanged)

### Structs

- `Wal` -- per-volume WAL state: active segment, sealed segments, pending HashMap, segment drain counts
- `WalEntry` -- 20 bytes: segment_id, data_offset, data_len, meta_offset, meta_len, created_at
- `MaterializeRequest` -- Arc<str> bucket + Arc<str> key + WalEntry (zero heap allocation on hot path)
- `MaterializeDispatch` -- round-robin sender across N worker channels
- `WalShardWriter` -- ShardWriter impl for streaming PUTs through the WAL

### Integration with LocalVolume

- `enable_wal()` creates WAL directory, opens WAL, spawns workers, runs recovery
- `open_shard_writer()` returns WalShardWriter when WAL is enabled
- `write_shard()` appends to WAL when enabled and object is below threshold
- `read_shard()` checks WAL pending before file tier
- `stat_object()` checks WAL pending before file tier
- `list_objects()` merges WAL pending keys with file tier keys
- `delete_object()` removes from WAL pending map
- `drain_pending_writes()` waits until WAL pending count reaches zero

### CLI

```
--write-tier wal       enable WAL on all local volumes
--write-tier file      default, no WAL (direct to file tier)
```

## Performance (L3, 1 disk, ftt=0, no write cache, Defender-excluded)

| Size | file | log | pool | **wal** |
|---|---|---|---|---|
| 4KB PUT p50 | 798us | 135us | 157us | **155us** |
| 64KB PUT p50 | 961us | 280us | 259us | **348us** |

The WAL is 5.1x faster than file tier at 4KB and competitive with
pool. The 20us gap vs log is the channel send + pending map insert --
the cost of having a background worker instead of permanent storage.

At 64KB the WAL is slower than pool because the WalShardWriter buffers
all chunks before appending. This is an area for optimization.

## Design decisions

**WAL is ephemeral, not permanent.** The log store made the log the
final resting place, which required GC. The WAL is a landing zone.
Objects leave as soon as they are materialized. No compaction needed.

**Worker reads from mmap, not from request.** The MaterializeRequest
carries only offsets (20 bytes), not data copies. The worker reads
data from the segment mmap when it is ready to write. This keeps the
PUT hot path allocation-free.

**Arc<str> for identity.** Bucket and key in MaterializeRequest use
Arc<str> (ref-counted, no heap allocation) instead of String (heap
allocation per PUT). The WAL pending map already uses Arc<str>, so
the request just clones the reference count.

**Size threshold.** Objects above 64KB go straight to file tier. The
write amplification of WAL (write to segment + write to final files)
is only worth it when filesystem metadata overhead is a significant
fraction of total write time. At large sizes, raw I/O dominates.
