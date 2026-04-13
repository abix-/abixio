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
1. Needle::new()              serialize ObjectMeta to msgpack
2. wal.lock()                 acquire Mutex (same as log store)
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

## Performance

### IMPORTANT: always benchmark in release mode

`cargo test` builds in debug (unoptimized) by default. Debug mode
inflates xxhash64 by 48x (192us vs 4us at 64KB) and mmap writes by
10x. Always use `cargo test --release` for benchmark tests or use
the `abixio-ui bench` harness (which is always release).

### Head-to-head (release, same process, same disk, non-interleaved)

| Size | wal | log | file |
|---|---|---|---|
| 4KB write_shard p50 | **3us** | **3us** | 878us |
| 64KB write_shard p50 | **30us** | **31us** | -- |

WAL and log are identical in release mode. Both are 293x faster
than the file tier at 4KB. The entire WAL-vs-log gap observed in
earlier debug-mode tests was compiler optimization overhead, not a
real code difference.

### 64KB step breakdown (release)

| Step | p50 |
|---|---|
| Needle::new (msgpack serialize) | 24us |
| segment.append (serialize_into mmap) | 26us |
| xxhash64 checksum (64KB) | 4us |
| msgpack serialize alone | <1us |
| **WAL write_shard total** | **30us** |
| **log write_shard total** | **31us** |

At 64KB, the mmap memcpy (26us) and needle construction (24us)
dominate. The WAL overhead (pending map + try_send) is invisible.

### L3 bench (abixio-ui, release, 1 disk, ftt=0, no write cache, Defender-excluded)

#### PUT p50

| Size | file | **wal** | log | pool |
|---|---|---|---|---|
| 4KB | 793us | **147us** | 143us | 159us |
| 64KB | 1.0ms | **326us** | 301us | 264us |

The L3 bench numbers are higher than the head-to-head because they
include VolumePool routing overhead (EC resolution, placement, quorum
checks). The relative ordering can vary between runs due to filesystem
state differences between sequential tier tests.

#### GET p50

| Size | file | wal | **log** | pool |
|---|---|---|---|---|
| 4KB | 452us | 497us | **33us** | 660us |
| 64KB | 496us | 501us | **52us** | 644us |

WAL GET is comparable to file tier because after materialization,
reads go through the file tier. Log GET is 15x faster because it
serves from the permanent in-memory index + mmap. This is the
tradeoff: the WAL trades fast GET for no GC, no permanent index,
and file-per-object layout.

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

**WAL is ephemeral, not permanent.** The log store made the log the
final resting place, which required GC. The WAL is a landing zone.
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

The log store tried to be both fast write AND fast read AND permanent
storage in one system. That forced it into GC, permanent in-memory
indexes, and startup rebuilds. Splitting the responsibilities into
three tiers eliminates all three problems.

## What the WAL replaces

The WAL supersedes both the log store and the write pool:

| Old system | Problem | WAL solution |
|---|---|---|
| log store (`log_store.rs`) | permanent log needs GC, RAM index scales with total data, startup rebuilds all segments | WAL is ephemeral, entries leave after materialize, startup only processes un-materialized entries |
| write pool (`write_slot_pool.rs`) | slot management, DashMap of pending renames, replenishment logic | WAL appends to a single mmap, no slots to manage |

The log store's GET advantage (33us vs 533us) will be addressed by
the planned read cache, not by keeping the log store alive.
