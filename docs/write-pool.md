# Pre-opened temp file pool

**Authoritative for:** write pool internals. Slot pool design, rename
worker, hot path optimizations, backpressure, crash recovery, read
path integration, and coexistence with the log store. If you need to
know how the pool works internally, this is the only doc.

**Not covered here:** how a PUT reaches the pool (see [write-path.md](write-path.md)),
pool development history and phase-by-phase benchmarks (see
[layer-optimization.md](layer-optimization.md)), benchmark results
(see [benchmarks.md](benchmarks.md)).

Each disk holds a small pool of already-open temp files. A PUT writes
shard bytes to one slot's data file and meta JSON to its companion
meta file, then acks. The mkdir, file creates, and the rename to the
destination all happen on a background worker after the client has
been told the PUT succeeded. Two syscalls on the hot path instead of
seven.

The motivation is GC simplicity. The log store keeps many objects per
segment file, requiring a compactor. The pool keeps one file per
object, so reclaiming space is just `unlink()`.

The pool and the log store are alternatives at the same tier level.
The [RAM write cache](write-cache.md) sits above whichever one is
enabled.


## Why

The current file-per-object write path, which the log store already
bypasses for objects <=64KB and the pool aims to replace entirely,
does this on every PUT, per disk:

```
tokio::fs::create_dir_all(obj_dir)              <- mkdir + parent walk + MFT write
tokio::fs::write(shard_path, data)              <- open + write + close (3 syscalls)
tokio::fs::write(meta_path, meta_json)          <- open + write + close (3 syscalls)
```

That's about seven syscalls minimum, plus directory entry updates.
On NTFS, measured (`docs/write-cache.md` baseline section):

```
mkdir + shard.dat + meta.json:   0.628ms   1593 obj/s
log store append:                0.002ms   544K obj/s
```

The 626us delta is filesystem metadata work, not the data itself. The
log store eliminated it for tiny objects by appending to one
always-open segment. The pool applies the same insight at the
granularity of one file per object: keep the files open in advance,
write to them on PUT, move them to their final location later. This
works for any size, including the small objects the log store
currently handles.

## How it works

### Per-disk slot pool

On `LocalVolume::new`, each disk creates a pool of N pre-opened slots
in `.abixio.sys/tmp/`. Default N is 32. Each slot is a pair of files:

```
.abixio.sys/tmp/
  slot-0000.data.tmp     <- pre-opened, empty, will receive shard bytes
  slot-0000.meta.tmp     <- pre-opened, empty, will receive meta JSON
  slot-0001.data.tmp
  slot-0001.meta.tmp
  ...
  slot-NNNN.data.tmp
  slot-NNNN.meta.tmp
```

The pool holds N `WriteSlot` values, each carrying both file handles
and both paths. A caller pops one, fills both files, and sends a
rename request to the per-disk async worker.

### PUT hot path

Only the bare minimum needed for correctness runs before ack: two
disk writes (for crash safety) and one DashMap insert (for
read-after-write consistency). Everything else is post-ack housekeeping.

```
PUT 1MB to bucket/key:

  pop slot from pool                            <- channel recv, ~100ns
    |
    v
  slot.data_file.write_all(shard_bytes)         <- 1 syscall
    |
    v
  slot.meta_file.write_all(meta_json)           <- 1 syscall
    |
    v   data is now durable on disk (page cache + recovery handles crash)
    |
  pending_renames.insert((bucket,key), entry)   <- DashMap insert, ~100ns
    |                                              required: makes the object
    |                                              visible to read-after-write
    v
  ack to client                                 total before ack: 2 syscalls
    |
    v   (after the client has been told)
  rename_tx.send(slot_id)                       <- non-blocking, worker-side hint
```

Two syscalls and one DashMap insert before ack. The mkdir, both file
creates, the close cycles, and the meta open+write+close have all
moved off the request path. So has the channel send to the rename
worker. A crash before the send is harmless because recovery picks
up the orphaned temp files on next startup.

**Why the DashMap insert can't move after ack.** Without it in the
pre-ack path, this sequence becomes possible: client PUTs, gets 200
OK, immediately GETs the same key, hits an empty `pending_renames`
(insert hasn't happened yet), falls through to the file tier (worker
hasn't renamed yet), gets a 404. Read-after-write consistency is an
S3 guarantee; the existing log store and RAM write cache also
populate their in-memory index before acking, for the same reason.

For the streaming `LocalShardWriter` path, `open_shard_writer` returns
a writer that holds the slot internally. `write_chunk` writes to the
slot's data file. `finalize` writes the meta JSON, inserts pending,
sends to the rename queue. Same overhead beyond the chunk writes
themselves.

### Hot path optimizations

The two writes can be made meaningfully faster without changing the
design. The recommendations below are the **measured** result from
Phase 2 (`bench_pool_l1_slot_write`); see the Phase 2 section under
"Implementation status" for the full numbers.

**1. Run the two writes concurrently with `tokio::try_join!`.**
**MEASURED 5.5x speedup at 4KB.**

The data write and the meta write target two different file
descriptors; nothing forces them to be sequential. `tokio::fs`
dispatches each `write_all` to the blocking thread pool, so
concurrent writes really do run on two threads in parallel.

```rust
tokio::try_join!(
    data_file.write_all(shard_bytes),
    meta_file.write_all(meta_json),
)?;
```

Wall-clock cost becomes `max(data, meta)` instead of `data + meta`.
Phase 2 measured 33us serial vs 6.3us joined at 4KB. The savings is
larger than just the meta write time, because the tokio blocking-pool
dispatch overlaps too.

**2. simd-json compact output instead of `serde_json::to_vec_pretty`.**
**MEASURED 30% faster than serde_json, ~35% smaller on disk.**

Phase 2.5 measured four serializers in isolation. `simd_json::serde::to_vec`
won at 383ns avg vs `serde_json::to_vec_pretty` at 858ns: a 55%
speed win combined with 35% smaller output. Round-trip parses through
`serde_json::from_slice` cleanly so destination meta.json files stay
fully compatible with the rest of the codebase.

```rust
let meta_json = simd_json::serde::to_vec(&meta_file)?;
slot.meta_file.write_all(&meta_json).await?;
```

`sonic-rs` was measured and rejected (13% slower than serde_json at
this payload size). `serde_json::to_vec` (compact) is the fallback
if simd-json isn't available on the target.

The absolute win is small (~475ns vs the current pretty path), but
on a sub-microsecond serialization step the percentage matters. See
Phase 2.5 results under "Implementation status" for the full table.

**3. Sync `std::fs::File::write_all` for small payloads.**
**REJECTED by Phase 2 measurement.** Predicted to save 2-4us per
small PUT; actually measured **14x slower** than the tokio async
path at 4KB (86us vs 6us). Dropped. The cause may be a measurement
artifact in `tokio::fs::File::into_std()`, or the tokio blocking
pool may genuinely be faster than direct syscalls on Windows for
this size class. Either way, the verdict is clear: don't ship.

**Net hot path (Phase 4 will use this):**

```rust
let WriteSlot { mut data_file, mut meta_file, .. } = pool.try_pop().unwrap();
let meta_json = simd_json::serde::to_vec(&meta_file)?;
tokio::try_join!(
    data_file.write_all(&shard_bytes),
    meta_file.write_all(&meta_json),
)?;
```

Measured 4KB pre-ack path: ~6us through the pool vs ~776us through
the file tier. **130x faster** on the hot path itself, before any
read-path or worker integration. simd-json saves another ~161ns
on the meta serialization step.

### Async rename worker (one task per disk)

```
loop {
  recv RenameRequest from channel
    |
    v
  look up PendingEntry from pending_renames
    |
    v
  tokio::fs::create_dir_all(destination_dir)    <- mkdir
    |
    v
  tokio::fs::rename(slot.data_path, final_shard_path)
    |
    v
  tokio::fs::rename(slot.meta_path, final_meta_path)
    |
    v
  pending_renames.remove((bucket, key))
    |
    v
  open a fresh slot to replace the consumed one:
    open new slot-NNNN.data.tmp
    open new slot-NNNN.meta.tmp
    push the new WriteSlot back into the pool
}
```

Worker syscalls per drained PUT: mkdir + 2 renames + 2 file creates
(opening the new slot) = 5, all off the request path. **No parsing.
No copying. No re-serialization.** The two renames are not jointly
atomic; the crash window is handled by recovery (next section).

Opening the two new slot files is the only file-create work that
happens. It runs in the background between requests, not on the
request path.

## `ObjectMetaFile` format change

For the worker to be a literal `mv` (no parsing, no rewriting), the
temp meta file must be byte-for-byte the destination meta.json.
That means the meta file has to know its own destination: which
bucket and which key it belongs to.

`ObjectMeta` already carries `version_id`, `is_latest`, and
`is_delete_marker`. What it doesn't carry is `bucket` and `key`.
Today these are encoded in the directory path, so the meta file
doesn't need them. With the pool, we want the meta file to be
self-describing so crash recovery can find the destination from the
file alone.

Add two fields to `ObjectMetaFile`:

```rust
pub struct ObjectMetaFile {
    #[serde(default)]
    pub bucket: String,        // NEW
    #[serde(default)]
    pub key: String,           // NEW
    pub versions: Vec<ObjectMeta>,
}
```

`#[serde(default)]` makes the change backward-compatible in both
directions:

- Old meta.json files (no `bucket`, no `key`): deserialize fine,
  fields default to empty string. Existing code that derives bucket
  and key from the path is unchanged.
- New meta.json files (with `bucket`, `key`): old code accepts the
  unknown fields silently. New code can read the field directly.
- The temp meta file is the same format as the destination meta.json.
  No special pool format. The worker is a literal `mv`.

These are not wrapper hacks. They are proper identity fields. The
public `ObjectInfo` struct already carries `bucket` and `key` (see
`metadata.rs:78`); they were just absent from `ObjectMetaFile`
because the path encoded them. The pool design makes that path
dependency obsolete.

Cost: ~50 bytes per object on disk in meta.json. Negligible.

## Crash recovery: stateless

The two renames are not jointly atomic. There are three possible
crash points:

1. **Crash before the first rename.** Both temp files intact in
   `.abixio.sys/tmp/`. Recovery scans, reads `bucket` and `key`
   from `slot-N.meta.tmp`, runs both renames. Recovered.
2. **Crash between the two renames.** `shard.dat` is at the
   destination, `meta.tmp` is still in the temp dir. Recovery scans
   the temp dir, reads the meta file to find the destination, sees
   the data file already in place, runs only the meta rename.
   Recovered.
3. **Crash after both renames, before the pool gets a fresh slot.**
   Both files at the destination, no temp files left. Recovery sees
   no temp pair to process. The pool init step at startup creates
   fresh slots. Recovered.

In all three cases the destination ends up with a complete
`shard.dat + meta.json` pair. **Zero data loss for any acked PUT
under process crash.**

The recovery procedure on `LocalVolume::new`, before creating the
pool:

1. List `.abixio.sys/tmp/` and group files by slot id.
2. For each `slot-N.meta.tmp` that is non-empty and parseable as
   `ObjectMetaFile`:
   a. Read `bucket` and `key` (and `versions[0].version_id` if
      versioned) to determine the destination.
   b. mkdir the destination.
   c. If `slot-N.data.tmp` exists: rename it to the final shard path.
   d. Rename `slot-N.meta.tmp` to the final meta path.
3. For orphans:
   - `slot-N.data.tmp` with no `slot-N.meta.tmp`: data was written
     but the meta write didn't complete. The PUT was never acked.
     Delete.
   - `slot-N.meta.tmp` that fails to parse: incomplete write. Delete.
4. Delete any other leftovers in `.abixio.sys/tmp/`.
5. Create N fresh slot pairs and populate the pool.

This is **stateless**: the temp meta files themselves are the source
of truth for which destination each pending PUT belongs to. The
in-RAM `pending_renames` table is purely a hot-path read cache; on
crash it is rebuilt from disk by the recovery scan.

Power loss is the only failure mode that loses data, same as the
existing log store and same as MinIO/RustFS no-fsync writes.
Documented and accepted.

## Read path integration

Every reader checks `pending_renames` before falling through to the
file tier. This is the same fall-through pattern that already exists
for log store lookups in `local_volume.rs:343-466`:

- `read_shard(bucket, key)`:
  - Check `pending_renames`. If hit, read the data file by path
    (`tokio::fs::read`). Return `(data, entry.meta.clone())`.
  - Otherwise check log store (if enabled), then fall through to the
    file tier.
- `mmap_shard(bucket, key)`:
  - If pending: open the temp data file by path, mmap it, return
    `MmapOrVec::Mmap`.
  - Otherwise log store / file tier.
- `stat_object(bucket, key)`:
  - If pending: return `entry.meta.clone()` directly. **Zero disk
    hits.**
  - Otherwise log store / file tier.
- `list_objects(bucket, prefix)`:
  - Walk the file tier (existing behavior).
  - Union the log store keys (existing behavior).
  - Union pending keys for this bucket+prefix.
- `delete_object(bucket, key)`:
  - If pending: send a `Cancel` to the rename worker for that slot
    id. The worker drops the pending entry, deletes both temp files,
    and creates a fresh slot to replace it.
  - Otherwise log store / file tier.

A subtle race: if a GET arrives just as the rename worker is mid-rename,
the path could 404. Protect with a small retry: if `pending_renames`
says present but the data file at the temp path is gone, also check
the final file path before returning 404.

## Backpressure

- **Pool empty (no slots available).** Caller falls through to the
  existing slow path. Graceful degradation. The pool gets fresh
  slots back as the rename worker finishes work.
- **Rename queue depth above 256 entries per disk.** Caller falls
  through to the slow path. Prevents the rename worker from falling
  arbitrarily behind under sustained load.
- **New slot creation failure.** If the worker can't open a new slot
  file (disk full, permission denied), the pool runs at degraded
  depth and the slow path takes over.

No client-side errors in any of these cases. The pool is a fast path,
not a required path.

## Trade-offs

1. **GET is no faster than today's file tier (worse than log store).**
   The pool's win is purely on the write path. A GET that misses
   `pending_renames` falls through to `mmap_shard`, which is
   `File::open + mmap` at ~100us on NTFS. The log store does
   HashMap lookup + slice from an already-mmap'd segment at ~10us.
   For tiny objects the log store still wins on reads. Benchmarks
   must measure read latency before deciding to drop the log store.

2. **Two files per slot doubles the temp dir file count.** N=32 means
   64 files in `.abixio.sys/tmp/`. Plus up to 256 in-flight
   pairs pending rename. Maximum ~640 files per disk. NTFS handles
   this fine; not a concern.

3. **Two non-atomic renames.** Crash window is handled by recovery
   (see above). Tested explicitly by the integration test that
   kills the process between the two renames and verifies recovery
   produces a complete object.

4. **Versioned writes still need read-modify-write of meta.json.**
   The current `LocalShardWriter::finalize` versioned branch reads
   the existing meta.json, inserts a new version at the front, and
   writes the merged file back. The pool's rename worker has to do
   the same, which means the worker can't be a pure `mv` for
   versioned writes. It does the read-modify-write at rename time,
   not on the request path. Acceptable: it's serialized per-key,
   not globally, and it's off the hot path.

5. **Replenish cost.** On every consumption the worker opens 2 new
   files. ~2 file creates per drained PUT, all off the request
   path. Bounded by worker throughput.

6. **Streaming uploads hold a slot for the entire upload.** A
   multipart upload of 1GB holds one slot for several seconds.
   Pool size 32 means at most 32 concurrent streaming uploads per
   disk before fallback. Fine for current scale; tune later if
   needed.

7. **`ObjectMetaFile` format change.** Adding `bucket` and `key` is
   a one-time format change. Backward compatible in both directions
   thanks to `#[serde(default)]`. ~50 bytes per object on disk.

## Coexistence with the log store

**The pool is not a log store replacement.** This was the original
design goal, and the storage-layer benchmarks (Phase 4.5 / Phase 5.6)
supported that goal. Phase 8 measured the gap end-to-end and the
conclusion flipped.

Phase 8 results at 4KB through the full HTTP stack:

| Operation | file | log | pool |
|---|---|---|---|
| PUT p50 | 1406us | **295us** | 942us |
| GET p50 | 582us | **168us** | 761us |

The log store is **3.2x faster** than the pool at 4KB PUT and
**4.5x faster** at 4KB GET. The pool's storage-layer write-path
optimizations exist but are dwarfed by the ~930us of HTTP stack
overhead that applies to all three tiers equally. For reads, the
pool falls through to `File::open + mmap` (~100us syscall floor)
while the log store does HashMap lookup and slice from a
permanently-mmap'd segment. The design doc has flagged this GET
trade-off from day one. Phase 8 put a number on it.

**The pool's actual sweet spot is 1MB to 10MB.** That's where the
log store has fallen through to the file tier (it only handles
<=64KB) and the pool's rename trick still pays off:

| Size | file PUT p50 | log PUT p50 | pool PUT p50 |
|---|---|---|---|
| 1MB | 4200us | 4057us | **3623us** |
| 10MB | 17910us | 37003us | **16582us** |

At 10MB the pool beats the log store by **2.2x** because the log
store has degraded to file-tier fallback, and it beats the file
tier by **1.1x** because the pool's temp-file write is only
marginally cheaper than a direct write at that size.

**At 100MB the file tier beats the pool.** PUT p50: file 152ms,
pool 257ms. The 100MB temp-file rename on NTFS adds ~100ms that
the file tier never pays. Use the file tier for huge objects.

**The right ship plan, per Phase 8:**

- `<=64KB`: log store
- `64KB to 10MB`: pool
- `>10MB`: file tier

Phase 7 (admin endpoint), Phase 9 (concurrency bench), and the
`--write-tier` CLI wiring all still need to land before this
becomes a shippable default. But the per-size tier decision is no
longer an open question at the sequential-sequential baseline.

Runtime flag (still planned, still right):

```
--write-tier log     # log store + file tier (current behavior)
--write-tier pool    # pool for everything, log store disabled
--write-tier file    # both disabled, baseline slow path
```

`--write-tier=pool` is the all-pool mode for benchmarking only.
The production default remains the three-tier handoff above.

## Development history

Pool development phases (Phase 1 through Phase 8.7) are documented in
[layer-optimization.md](layer-optimization.md). That includes the
phase-by-phase benchmarks, slot write strategy comparisons, JSON
serializer shootout, rename worker scaling tests, and the end-to-end
three-tier matrix that shaped the current defaults.
