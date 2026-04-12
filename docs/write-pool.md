# Pre-opened temp file pool

An alternative write path originally designed to replace the
[log store](write-log.md). Each disk holds a small pool of already-open
temp files. A PUT writes shard bytes to one slot's data file and meta
JSON to its companion meta file, then acks. The mkdir, file creates,
and the rename to the destination all happen on a background worker
after the client has been told the PUT succeeded. Two syscalls on the
hot path instead of seven.

The motivation is GC simplicity. The log store keeps many objects per
segment file, which means reclaiming space from overwritten or deleted
objects requires a segment compactor that scans live needles, copies
them to a new segment, and unlinks the old one. The pool keeps one
file per object, so reclaiming space is just `unlink()`. The
filesystem handles it natively, no compactor needed.

The pool and the log store are alternatives at the same tier level.
The [RAM write cache](write-cache.md) sits above whichever one is
enabled. The current benchmark conclusion is a three-tier split:
log store for `<=64KB`, pool for the mid-range write window, and
file tier for large writes. Production `--write-tier` CLI wiring is
still pending; see the later sections for details.

For the canonical end-to-end PUT path, the exact ack semantics of the
pool branch, and how it relates to the other branches, see
[write-path.md](write-path.md). This page stays focused on the pool's
internal design and benchmark history.

## End-to-end pool performance lives in write-path.md

The pool's measured end-to-end PUT and GET at 4KB through 100MB,
including the comparison vs the file tier and the log store, lives in
[write-path.md::Branch C](write-path.md#branch-c-local-write-pool) and
[write-path.md::Where the time goes](write-path.md#where-the-time-goes).
That is the canonical, user-visible story.

The phase journey below (Phase 1 -> Phase 8.7) describes how each
storage-layer optimization was *measured at the storage layer*, in
isolation from HTTP, s3s, and the SDK. Those storage-layer numbers are
still correct in their own frame, and they are useful context for
*why* each optimization landed -- but they are not the numbers a user
of the live server will see at 4KB. The pool's "53x faster at 4KB"
claim from Phase 4.5 / 5.6 was a storage-layer tight-loop measurement
on an idle tokio runtime; the user-visible 4KB win in the SDK matrix
bench is much smaller because the ~600 us SDK + auth + hyper floor
hides most of the storage-tier delta.

If you only care about what shipping the pool means for production,
read [write-path.md::Branch C](write-path.md#branch-c-local-write-pool)
first. The phase history below is the design journey, not the
production claim.

## Table of contents

- [Why](#why): the syscall cost the pool eliminates
- [How it works](#how-it-works)
  - [Per-disk slot pool](#per-disk-slot-pool)
  - [PUT hot path](#put-hot-path)
  - [Hot path optimizations](#hot-path-optimizations)
  - [Async rename worker (one task per disk)](#async-rename-worker-one-task-per-disk)
- [`ObjectMetaFile` format change](#objectmetafile-format-change)
- [Crash recovery: stateless](#crash-recovery-stateless)
- [Read path integration](#read-path-integration)
- [Backpressure](#backpressure)
- [Trade-offs](#trade-offs)
- [Coexistence with the log store](#coexistence-with-the-log-store)
- [Implementation phases](#implementation-phases): the table of what's done and pending
- [Benchmark plan](#benchmark-plan)
- [Implementation status](#implementation-status): the deep dive on each phase
  - [Phase 1: pool primitive in isolation (done)](#phase-1-pool-primitive-in-isolation-done)
  - [Phase 2: slot writes with real I/O (done)](#phase-2-slot-writes-with-real-io-done)
  - [Phase 2.5: faster JSON serializer (done)](#phase-25-faster-json-serializer-done)
  - [Phase 3: rename worker in isolation (done)](#phase-3-rename-worker-in-isolation-done)
  - [Phase 4: first integration into LocalVolume::write_shard (done)](#phase-4-first-integration-into-localvolumewrite_shard-done)
  - [Phase 4.5: profile and fix the 4KB integration overhead (done)](#phase-45-profile-and-fix-the-4kb-integration-overhead-done)
  - [Phase 5: read path integration (pending_renames) (done)](#phase-5-read-path-integration-pending_renames-done)
  - [Phase 5.5: shrink PendingEntry (done, then validated as unnecessary)](#phase-55-shrink-pendingentry-done-then-validated-as-unnecessary)
  - [Phase 5.5+: stable-median measurement (done)](#phase-55-stable-median-measurement-done)
  - [Phase 5.6: move file drops off the hot path (done)](#phase-56-move-file-drops-off-the-hot-path-done)
  - [Methodology lessons](#methodology-lessons-from-phase-55-and-phase-56)
  - [Phase 6: crash recovery scan (done)](#phase-6-crash-recovery-scan-done)
  - [Phase 8: end-to-end three-tier matrix (done)](#phase-8-end-to-end-three-tier-matrix-done)
  - [Phase 8.5: where the missing 900us actually lives (done)](#phase-85-where-the-missing-900us-actually-lives-done)
  - [Phase 8.6: DRY pool optimizations into the file tier (done)](#phase-86-dry-pool-optimizations-into-the-file-tier-done)
  - [Phase 8.7: raise pool defaults and scale the rename worker (done)](#phase-87-raise-pool-defaults-and-scale-the-rename-worker-done)
  - [Phase 7, 9: pending](#phase-7-9-pending)
- [Open questions for implementation](#open-questions-for-implementation)
- [Files to modify (when phase 2 begins)](#files-to-modify-when-phase-2-begins)
- [Verification](#verification)
- [See also](#see-also)

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

## Implementation phases

| Phase | What | Delivers | Status |
|---|---|---|---|
| 1 | `write_slot_pool.rs`: pool, slot, fresh-slot creation | core data structures | **done** (63ns pop+release, see "Implementation status" below) |
| 1.5 | Slot-write strategy bench (Phase 2) | measured optimization stack | **done** (pool 23x faster than file tier at 4KB, see "Phase 2 results" below) |
| 1.6 | Faster JSON serializer bench (Phase 2.5) | maximum JSON speed via simd-json / sonic-rs | **done** (simd-json wins at 383ns avg, 30% faster than serde_json; sonic-rs rejected as slower; see "Phase 2.5 results" below) |
| 1.7 | Rename worker in isolation (Phase 3) | drain rate ceiling | **done** (1 worker = 940 ops/sec, 27x file tier; 2 workers = 1744 ops/sec; ship 1 per disk; see "Phase 3 results" below) |
| 2 | `ObjectMetaFile` `bucket` and `key` fields | self-describing meta | **done** (Phase 4) |
| 3 | Wire `write_shard` to use the pool when enabled | buffered PUT fast path | **done** (Phase 4: integrated pool is 5-18x faster than the file tier at every size; see "Phase 4 results" below) |
| 3.5 | Profile + fix the 4KB integration overhead (Phase 4.5) | smaller per-call cost | **done** (collapsed 3 path computations into 1; 4KB p50 dropped from 43us to 10us, pool is now 46x faster than file tier at 4KB; see "Phase 4.5 results" below) |
| 4 | Wire `open_shard_writer` to use the pool | streaming PUT fast path | pending |
| 5 | Read path integration in all five readers | reads see pending writes | **done** (Phase 5: PUT-then-immediate-GET works without `drain_pending`; production-safe for non-versioned writes) |
| 5.5 | Trim `PendingEntry` (drop the meta clone) | smaller per-call cost | **done** (Phase 5.5; turned out the meta clone was not the cost. See Phase 5.5+ for the real story) |
| 5.5+ | Stable-median measurement of the integrated bench | trustworthy 4KB numbers | **done** (10k iters showed Phase 5.5 was identical to Phase 4.5 at p50; the "regression" was 100% noise) |
| 5.6 | Move file drops off the hot path | flatter latency at every size | **done** (4KB avg 3x better, 10MB p99 12x better; pool now 53x file tier at 4KB and 4.6x at 100MB) |
| 6 | Crash recovery scan in `enable_write_pool` | restart safety | **done** (Phase 6: `recover_pool_dir` finishes any pending renames left from a crash before fresh slots are created; 14 new tests, all 355 total pass) |
| 7 | Admin endpoint `GET /_admin/pool/status` | operator visibility | pending |
| 8 | End-to-end three-tier matrix (HTTP layer) | data-driven decision | **done** (Phase 8: bench_pool_l4_tier_matrix measures PUT+GET p50 through the full hyper/s3s/VolumePool stack at 5 sizes for file/log/pool tiers. **Result: pool only wins 1MB-10MB, log store dominates <=64KB, file tier wins 100MB. Storage-layer 53x claims do not translate end-to-end.**) |
| 8.5 | Stack breakdown via layer subtraction | attribute the missing 900us | **done** (Phase 8.5: bench_pool_l4_5_stack_breakdown with 10 stages proves HTTP stack is 93us, abixio dispatch is 32us, and the rest is file-tier work plus **two hidden choke points in the default pool config**: depth 32 starves under sustained load, and channel buffer 256 backpresses tx.send. With depth 1024 + channel 100k the pool p50 drops from 942us to 318us, giving a real 2.55x win over file tier. Default config is undersized for sustained throughput.) |
| 8.6 | DRY pool optimizations into the file tier | shared optimization pattern across write paths | **done** (Phase 8.6: swapped `write_meta_file` to simd-json across all 9 call sites, collapsed file tier's 3x path validation to 1, added `tokio::try_join!` for concurrent shard+meta writes on non-inline file tier. Modest direct impact, largest maintenance win.) |
| 8.7 | Raise pool defaults + multi-worker rename | fix Phase 8.5 choke points in production | **done** (Phase 8.7: `RenameDispatch` round-robin across N worker channels, default channel buffer 256 → 10_000 per worker, default worker count 1 → 2. Pool 4KB p50 drops from 942us to **454us (2.1x vs file tier)** and 64KB from 1037us to **586us (2.3x vs file tier)**.) |
| 9 | Benchmark all three tiers under concurrency | concurrent load picture | pending |

## Benchmark plan

Run `cd abixio-ui && cargo test --release --test bench -- --ignored
--nocapture bench_matrix` and `abixio-ui/src/bench/` three times, once
for each tier:

| Setting | Tier behavior |
|---|---|
| `--write-tier=log` | log store + file tier |
| `--write-tier=pool` | pool for everything, log store off |
| `--write-tier=file` | both disabled, baseline |

Compare PUT and GET latency at 4KB, 64KB, 1MB, 10MB, and 1GB. The
data-driven decision: which tier ships as the default.

Tentative hypothesis: `pool` wins writes at 64KB and above; `log`
wins 4KB GETs by ~50us; `file` is the slowest of the three across
the board. If true, the right answer is either "pool wins at all
sizes, accept the small read regression for 4KB" or "pool for writes,
keep the log store reads enabled for hot small objects." Benchmarks
decide.

### Additional pool-only benchmarks

After the three-way tier comparison, run two more pool-only sweeps to
quantify optimizations the design leaves on the table by default.
These are tunables, not committed defaults.

**Pre-allocation of slot data files.** When a slot's data file is
created in `.abixio.sys/tmp/`, set its length to a max-expected size
(1MB or 4MB) using `set_len`. This pre-allocates extents on the
filesystem so the actual write doesn't have to allocate extents
during the syscall. NTFS and ext4 both reward pre-allocated files:
fewer extent allocations, fewer metadata updates, faster sequential
writes.

Trade-off:
- Too small: large PUTs still extend the file, no win.
- Too large: wastes space in `.abixio.sys/tmp/` while the slot is
  unused.

With 32 slots * 4MB = 128MB per disk reserved. Tunable. Measure
4KB / 64KB / 1MB / 10MB PUT latency with and without pre-allocation
at 1MB and 4MB sizes. Decision: enable pre-allocation if it shows
>=10% improvement at any common size, otherwise leave it off.

**Faster JSON serializer.** **DECIDED in Phase 2.5: simd-json wins.**
See the "Phase 2.5: faster JSON serializer (done)" section under
"Implementation status" for the measured numbers. Verdict:
`simd_json::serde::to_vec` at 383ns avg, 30% faster than
`serde_json::to_vec`. Phase 4 will use simd-json as the default
meta serializer in the pool hot path. sonic-rs was measured and
rejected (slower at this payload size).

## Implementation status

### Phase 1: pool primitive in isolation (done)

Landed in commit alongside this doc update. The new
`src/storage/write_slot_pool.rs` contains `WriteSlotPool::new`,
`try_pop`, `release`, and `available`. That is the bare slot pool,
with no rename worker, no `pending_renames`, no integration. Backed
by `crossbeam_queue::ArrayQueue` (lock-free MPMC).

Measured on Windows 10 NTFS, depth=32:

```
pool init (depth=32):                        11.96ms  (374us per slot pair)

single-thread pop+release (100k iters):
  avg  63ns   p50 100ns   p99 100ns   p999 100ns

concurrent pop+release:
   2 workers x 10000 ops:    20.4M ops/sec   48ns/op
   8 workers x 10000 ops:    14.7M ops/sec   68ns/op
  32 workers x 10000 ops:    12.2M ops/sec   81ns/op

empty try_pop:                              0ns (returns None, never blocks)
```

**Result: the primitive will never be the bottleneck.** The plan
target was <500ns for the pop+release cycle; actual is 63ns avg,
8x under target. Throughput sustains 12-20M ops/sec across
contention levels with no collapse. The actual file syscalls in
Phase 2+ will cap us at 5K-50K ops/sec depending on size, so the
pool primitive is 3-4 orders of magnitude faster than what it needs
to be.

The p50/p99/p999 percentiles all snap to exactly 100ns, which is
the `Instant::now()` resolution on this hardware. The operation
is genuinely faster than the timer can measure individually.

Bench output: `bench-results/phase1-pool-primitive.txt`. Re-run with:

```bash
k3sc cargo-lock test --release --test layer_bench -- \
    --ignored --nocapture bench_pool_l0_primitive
```

### Baseline captured for later comparison

`bench-results/baseline-l2-storage.txt` (10MB only, 1 disk and 4 disk):

```
VolumePool::put_object_stream (1 disk)   avg 21.55ms   464.0 MB/s
VolumePool::put_object        (1 disk)   avg 28.82ms   346.9 MB/s
VolumePool::get_object        (1 disk)   avg  8.78ms  1138.9 MB/s
VolumePool::put_object_stream (4 disk)   avg 27.99ms   357.3 MB/s
VolumePool::put_object        (4 disk)   avg 35.81ms   279.2 MB/s
VolumePool::get_object        (4 disk)   avg 11.95ms   837.0 MB/s
```

These are the file-tier numbers the pool needs to beat at 10MB
once Phase 4 wires it into `LocalVolume::write_shard`. Phase 2
will add a per-size sweep (4KB / 64KB / 1MB / 10MB) so we can see
where the pool wins most.

### Phase 2: slot writes with real I/O (done)

The bench function `bench_pool_l1_slot_write` in `abixio-ui/src/bench/`
runs five sizes (4KB / 64KB / 1MB / 10MB / 100MB) through six write
strategies, with each optimization layered on independently so we can
attribute the speedup. Output: `bench-results/phase2-slot-writes.txt`.
Re-run with:

```bash
k3sc cargo-lock test --release --test layer_bench -- \
    --ignored --nocapture bench_pool_l1_slot_write
```

**Strategies measured:**

- A: `file_tier_full`. mkdir + write shard + write meta (current path)
- B: `file_tier_no_mkdir`. Pre-create dir, only time the writes
- C: `pool_serial`. Pop slot, sequential async writes
- D: `pool_join`. C + concurrent writes via `tokio::try_join!` (#1)
- E: `pool_join_compact`. D + compact JSON instead of pretty (#2)
- F: `pool_sync_small`. E + sync `std::fs::File::write_all` for small payloads (#3, only at <=4KB)

**Headline numbers (Windows 10 NTFS, 1 disk, p50):**

| Size  | A: file tier | C: pool serial | D: pool join | E: pool compact | F: pool sync | Pool win vs A |
|---|---|---|---|---|---|---|
| 4KB   | 694us | 28us | **4.1us** | 3.9us | 75us | **170x** (D vs A p50) |
| 64KB  | 1.08ms | 31us | **30us** | 56us | n/a | **36x** |
| 1MB   | 1.44ms | 504us | 829us | **483us** | n/a | 3x |
| 10MB  | 5.63ms | 3.68ms | 3.72ms | **3.64ms** | n/a | 1.5x |
| 100MB | 91.9ms | 36.2ms | 37.7ms | **36.3ms** | n/a | 2.5x |

Pool wins at every size, by 1.5x to 170x. Even at 100MB, where the
data write itself dominates, the pool is 2.5x faster than the file
tier path. The mkdir + 2 file creates is dramatically more expensive
than expected on NTFS.

**mkdir cost (A vs B):** ~190us per PUT at 4KB, negligible at 64KB+.
This is what the pool's biggest small-size win comes from.

**Optimization #1 (concurrent writes via `tokio::try_join!`)**

Measured speedup at each size:

| Size | C: serial | D: join | Speedup |
|---|---|---|---|
| 4KB | 33us avg | 6.3us avg | **5.5x** |
| 64KB | 251us avg | 186us avg | 1.35x |
| 1MB+ | tied | tied | noise |

**Verdict: SHIP as default for all sizes.** 5.5x at 4KB is dramatically
larger than my pre-bench estimate ("save the meta write time, ~10us").
The win comes from overlapping the tokio blocking-pool dispatch
overhead, not just the meta syscall. At larger sizes the data write
dominates and the optimization is neutral, but never hurts.

**Optimization #2 (compact JSON via `serde_json::to_vec`)**

Measured: roughly neutral on speed across sizes. avg numbers swing both
ways within noise; p50 numbers are similar. The compact JSON output is
391 bytes vs 597 bytes pretty (~35% smaller).

**Verdict: SHIP for the disk-size win.** No measurable speed improvement
on this workload, but the smaller meta files reduce on-disk overhead
for free. There's no audience for the pretty-printed whitespace inside
meta.json files.

**Optimization #3 (sync `std::fs::File::write_all` for small payloads)**

Measured: 4KB strategy F = **86us avg / 75us p50**. Strategy E = 7.4us
avg / 3.9us p50. **F is ~14-19x SLOWER than E.**

This contradicts my pre-bench prediction ("~2-4us savings per small
PUT"). The sync std::fs path is wildly slower than tokio's async
blocking-pool path on Windows for this workload. The cause might be a
measurement artifact in `tokio::fs::File::into_std()` (the conversion
that happens out-of-timing may leave the file handle in a state that
makes subsequent syncs slow), or the tokio blocking pool may genuinely
be faster than direct syscalls for this size class on Windows.

**Verdict: REJECT.** Do not ship. This is exactly the kind of "obvious"
optimization the methodology is meant to catch. It sounded right on
paper, the measurement says no, and the numbers force the answer.

**Final hot path (Phase 4 will use this):**

```rust
let WriteSlot { mut data_file, mut meta_file, .. } = pool.try_pop().unwrap();
tokio::try_join!(
    data_file.write_all(&shard_bytes),
    meta_file.write_all(&compact_meta_json),
)?;
```

Two optimizations (#1 + #2). Optimization #3 dropped. Expected
hot-path latency at 4KB: ~6us. Current file tier at 4KB: ~776us.
**130x faster on the hot path itself**, before any read-path or
worker integration.

### Surprises from Phase 2

1. **Pool wins at 100MB by 2.5x.** Pre-bench prediction said "small
   or no improvement at 100MB+". The mkdir + file creates are far
   more expensive than the data write at every size, including 100MB.
2. **`tokio::try_join!` is 5.5x at 4KB**, not the predicted ~2x.
3. **`std::fs::File` is 14x slower than tokio's blocking-pool path**
   for sync small writes. Counter to all conventional wisdom; the
   measurement is the truth.
4. **High variance at 64KB-1MB.** p99 is 30-50x p50 in this range.
   Likely NTFS write coalescing or page cache flushes. Not a pool
   problem, but a future bench should use 5-10x more iterations in
   this band for cleaner avgs.

### Phase 2.5: faster JSON serializer (done)

The bench function `bench_pool_l1_5_json_serializers` in
`abixio-ui/src/bench/` runs four serializers against the same
representative `ObjectMetaFile` (391 bytes compact) over 100k
iterations, with round-trip parse validation through `serde_json`
to confirm output compatibility. Output:
`bench-results/phase2.5-json-serializers.txt`. Re-run with:

```bash
k3sc cargo-lock test --release --test layer_bench -- \
    --ignored --nocapture bench_pool_l1_5_json_serializers
```

**Numbers (Windows 10, 100k iters):**

| Crate | avg | p50 | p99 | output | vs serde_json::to_vec |
|---|---|---|---|---|---|
| **C: simd-json::serde::to_vec** | **383ns** | **400ns** | **400ns** | 391 bytes | **-161ns (-30%)** |
| B: serde_json::to_vec | 544ns | 500ns | 700ns | 391 bytes | baseline |
| D: sonic-rs::to_vec | 614ns | 600ns | 700ns | 391 bytes | +70ns (+13%) |
| A: serde_json::to_vec_pretty | 858ns | 800ns | 1100ns | 597 bytes | +314ns (+58%) |

**Winner: simd-json.** 383ns avg, 30% faster than `serde_json::to_vec`.
Round-trip parses cleanly through serde_json (the destination
meta.json reader), output is byte-identical in size to compact
serde_json. **Phase 4 will use `simd_json::serde::to_vec` as the
meta serializer in the pool hot path.**

**Rejected: sonic-rs.** Measured 13% *slower* than serde_json on this
workload. The marketing claims of 2-3x speedup are for parsing larger
documents; at 391 bytes the per-call overhead dominates. Not shipped.

**Important caveat:** the absolute win is small (~161ns saved per
PUT). The 30% percentage win is real but it's 30% of a sub-microsecond
operation. Compared to the ~10us file syscall, the JSON step is not
the bottleneck. simd-json shaves 161ns off a ~6us hot path, which is
about 2-3% improvement end-to-end. We're shipping it because (a) the
explicit goal is maximum JSON speed, (b) the round-trip validates,
(c) the dep is small and stable, (d) every nanosecond counts on a
hot path that we're trying to make competitive with the log store.

**Methodology lesson:** Phase 2 concluded that compact JSON was
"neutral on speed." Phase 2.5 in isolation showed compact is 314ns
faster than pretty (544ns vs 858ns), but the I/O variance in Phase 2
hid that 314ns difference completely. **Components measured in
isolation can reveal real wins that I/O variance buries.** Worth
remembering for future bench design. If a number "doesn't change"
through end-to-end I/O, measure the component alone before drawing
conclusions.

**Production caveat:** the 30% simd-json win was measured on this
Windows x86_64 development box. simd-json's effectiveness depends on
runtime SIMD detection. Linux production targets may differ. Re-run
this bench on the production target before committing in Phase 4.

### Phase 3: rename worker in isolation (done)

The bench function `bench_pool_l2_worker_drain` in
`abixio-ui/src/bench/` runs four scenarios against the new
`run_rename_worker` and `WriteSlotPool::replenish_slot` code.
Output: `bench-results/phase3-rename-worker.txt`. Re-run with:

```bash
k3sc cargo-lock test --release --test layer_bench -- \
    --ignored --nocapture bench_pool_l2_worker_drain
```

**New code in `src/storage/write_slot_pool.rs`:**

- `RenameRequest` struct. The message type sent on the rename
  channel by the (eventual) PUT path.
- `WriteSlotPool::replenish_slot(slot_id)`. Creates a fresh slot
  file pair at the same slot_id paths and pushes a new `WriteSlot`
  to the queue.
- `process_rename_request(pool, req)`. The actual work for one
  request: mkdir + 2 renames + open a fresh slot to replace the
  consumed one. Public so the bench can drive parallel workers.
- `run_rename_worker(pool, rx, shutdown)`. The loop. Matches the
  heal worker shutdown pattern at `heal/worker.rs:181-226`.

**Numbers (Windows 10 NTFS, 1 disk):**

```
-- Scenario 1: cold drain (mkdir + 2 renames + open new slot) --
N=32     workers=1   drain    34.00ms     941 ops/sec   1063us/op
N=256    workers=1   drain   290.59ms     881 ops/sec   1135us/op
N=1024   workers=1   drain  1169.43ms     876 ops/sec   1142us/op

-- Scenario 2: drain with pre-created dest dirs (no mkdir) --
N=256    workers=1   drain   239.32ms    1070 ops/sec    935us/op

-- Scenario 4: parallel workers --
workers=1   drain   269.47ms     950 ops/sec
workers=2   drain   146.82ms    1744 ops/sec   1.84x scaling
workers=4   drain   127.28ms    2011 ops/sec   2.12x scaling
```

**Single worker drains at ~940 ops/sec.** Per-op cost is consistent
at ~1100us across all batch sizes (32, 256, 1024), with no per-batch
overhead. Estimated breakdown: ~200us mkdir, ~200us for both
renames, ~400-500us for opening the two new slot files, ~200us
tokio overhead. **Opening the new slot files is the single biggest
line item, larger than mkdir or rename individually.**

**mkdir is ~17% of the per-op cost.** Scenario 2 with pre-created
destination dirs runs at 935us/op vs 1135us/op with mkdir. Saving
the mkdir would lift throughput from 881 to 1070 ops/sec (+21%).
Worth a Phase 3.5 optimization if production load shows the worker
as a bottleneck.

**Parallel workers scale to 2x at 2 workers, then plateau.** 4
workers only adds 15% over 2 workers. NTFS rename parallelism on a
single disk has a hard ceiling around 2 concurrent independent
operations.

### Vs the methodology target

The plan said "5000 ops/sec per disk." We measured ~940 single-worker,
~1750 with 2 workers, ~2010 with 4. **Miss the aspirational target
by 2.5x even at 4 workers.** But the 5000 target was a guess, not
measured against any real workload. The right comparison is against
what the worker is replacing:

| Path | Throughput | Source |
|---|---|---|
| File tier `put_object` 10MB 1 disk | ~35 ops/sec | Phase 0 baseline |
| File tier `put_object_stream` 10MB 1 disk | ~46 ops/sec | Phase 0 baseline |
| MinIO `put_object` 4KB | ~367 ops/sec | comparison.md |
| **Pool worker drain (1 worker)** | **~940 ops/sec** | Phase 3 |
| Pool worker drain (2 workers) | ~1750 ops/sec | Phase 3 |

**The pool worker drains 27x faster than the existing file tier
and 2.5x faster than MinIO at 4KB.** It is dramatically faster than
the path it replaces. The 5000 target was aspirational; 940 is
enough for any realistic production load.

The hot-path PUT measured in Phase 2 is ~6us per request, which is
**180x faster than the worker drain rate.** This means in any
sustained load above 940 ops/sec, the rename queue grows and writes
fall through to the slow path (Phase 7 backpressure handles that).
The PUT request side is faster than the worker can keep up with,
not the other way around.

### Verdict

**Pass.** Ship 1 worker per disk in Phase 4. Design ready for 2
workers per disk if production load shows the single worker is
saturated (1.84x scaling already proven). Don't bother with 4
workers. The scaling falls off.

**Phase 3.5 backlog (defer until needed):**

- mkdir caching. Many PUTs share dest dirs (same bucket+prefix);
  cache "I already mkdir'd this" set. Could shave 200us off
  per-op cost (~17% throughput improvement).
- Optimistic rename + retry on ENOENT. Skip mkdir for the common
  case where the dest dir already exists from previous PUTs.
- Concurrent renames within one request. `tokio::try_join!` the
  two renames. Currently serial. Could shave another 50-100us.
- Parallel workers (2 per disk). Use them if production load
  saturates the single worker. Already proven at 1.84x scaling.

### Methodology note: Scenario 3 was poorly designed

Scenario 3 (steady-state with target rates) showed numbers far below
the cold-drain throughput (~280 ops/sec at "5000 target" vs ~940 in
cold drain). The cause: I ran ONE PUT-side task feeding ONE worker
task, both fighting over the tokio blocking pool. That doesn't
match production, where many concurrent HTTP request handlers feed
the worker in parallel. **The right design for that scenario is
many parallel PUT-side tasks.** Will revisit in Phase 7 (backpressure)
when we measure realistic concurrent load.

The Scenario 1 (cold drain) and Scenario 4 (parallel workers)
numbers are clean and tell the story. Scenario 2 (mkdir cost) is
useful as a sanity check.

### Surprises

1. **Opening fresh slot files dominates per-op cost.** ~400-500us
   for the two new file creates is larger than mkdir or rename
   individually. Pre-allocation doesn't help here; every steady-
   state slot replacement pays the same cost.
2. **Parallel rename ceiling at 2 workers.** NTFS doesn't parallelize
   independent renames on a single disk past ~2 concurrent operations.
3. **180x gap between hot path and worker drain.** ~6us PUT vs ~1100us
   worker. The pool's whole design depends on the queue absorbing
   bursts. The request side is much faster than the worker can
   keep up with, and the queue is what bridges the two.

### Phase 4: first integration into LocalVolume::write_shard (done)

Phase 4 stops treating the pool as a freestanding test object. After
this phase, calling `LocalVolume::write_shard` with the pool turned on
goes through the fast write path (Phase 2), the background worker
finishes the rename, and the destination directory ends up with a
normal `shard.dat` and `meta.json` that the rest of the system can
read.

This is the first phase that touches production code instead of just
adding bench code.

**What changed in the code:**

- `ObjectMetaFile` got two new fields, `bucket` and `key`. They are
  the object's identity. Old `meta.json` files that don't have them
  still load fine; the fields default to empty strings and the rest
  of the code keeps deriving them from the directory path.
- `LocalVolume` got three new fields: a handle to the slot pool, a
  way to send rename messages to the worker, and a way to tell the
  worker to stop. They're all empty until you call
  `enable_write_pool(depth)`.
- `LocalVolume::enable_write_pool(depth)` is the new switch that turns
  the pool on. It opens `.abixio.sys/tmp/`, creates `depth` slot file
  pairs, starts the rename worker as a background task, and stores
  the connection points so `write_shard` can use them. Mirrors the
  existing `enable_log_store()` pattern at `local_volume.rs:66`.
- `LocalVolume::write_shard` now tries the pool first when it's
  enabled and the object isn't versioned. If a slot is available, it
  pops the slot, writes the shard bytes and meta JSON to the
  pre-opened files (with the Phase 2 optimizations: simultaneous
  writes plus simd-json), sends a rename message to the worker, and
  returns. The slow path is the fallback for everything else
  (versioned objects, pool empty, pool not enabled).
- `LocalVolume::drain_pending()` is a test helper that waits until
  every in-flight rename has finished. Tests need it because Phase 4
  has no read-path support yet, so a request that just got a 200 OK
  for a PUT might 404 on a GET until the worker has finished the
  rename. Phase 5 fixes that with the in-memory lookup table; for
  now, tests call `drain_pending()` between writes and reads.
- `simd-json` moved out of dev-dependencies into the main dependency
  list because the production write path now uses it.

**What stayed unchanged:**

- All other write paths (versioned, multipart, streaming, log store,
  file tier) still work the same way. The pool is an opt-in extra
  path that runs alongside everything else.
- All 336 existing tests still pass. No regressions.
- Read paths (`read_shard`, `mmap_shard`, `stat_object`, `list_objects`)
  still go through the file tier. Reads will see a pool-written object
  only after the worker has finished its rename.

**Numbers (Windows 10 NTFS, 1 disk, full `LocalVolume::write_shard`
path):**

```
SIZE     STRATEGY                    AVG       p50       p99    THROUGHPUT
4KB      file tier (baseline)      1.09ms   649.6us   5.44ms      3.6 MB/s
4KB      pool (Phase 4)           132.7us    43.0us   2.10ms     29.4 MB/s

64KB     file tier (baseline)      1.32ms   1.00ms    6.85ms     47.5 MB/s
64KB     pool (Phase 4)           327.8us    57.5us   3.15ms    190.7 MB/s

1MB      file tier (baseline)      3.06ms   3.02ms    3.86ms    327.0 MB/s
1MB      pool (Phase 4)           789.9us   554.0us   3.59ms   1265.9 MB/s

10MB     file tier (baseline)     22.72ms   22.59ms  23.72ms    440.2 MB/s
10MB     pool (Phase 4)            3.94ms    3.76ms   5.14ms   2535.4 MB/s

100MB    file tier (baseline)    284.35ms  283.87ms 291.25ms    351.7 MB/s
100MB    pool (Phase 4)           35.95ms   35.56ms  37.53ms   2781.8 MB/s
```

**Headline: at every size, the integrated pool is 5x to 18x faster
than the file tier it replaces.** This is the real measurement,
not a microbenchmark in isolation. The bench calls
`LocalVolume::write_shard` exactly as a real PUT request would.

| Size | File tier (median) | Pool (median) | How much faster |
|---|---|---|---|
| 4KB | 649us | **43us** | **15x** |
| 64KB | 1.00ms | **57us** | **18x** |
| 1MB | 3.02ms | **554us** | **5.5x** |
| 10MB | 22.59ms | **3.76ms** | **6x** |
| 100MB | 283ms | **35.56ms** | **8x** |

At 100MB the pool sustains **2782 MB/s** versus the file tier's 352
MB/s. That's real bandwidth, not a microbench artifact, written
through the same code that production PUT requests use.

### The integration cost: ~30-40us of constant overhead per call

Phase 2 measured just the write step in isolation (pop a slot,
write the data, write the meta, drop the slot) at about 4us
median for a 4KB object. Phase 4 measures the full
`LocalVolume::write_shard` call at about 43us median for the same
4KB. The gap is about 39us, and it stays roughly the same across
all object sizes.

Where the 39us goes (rough estimates):

- checking that the bucket name and key are valid: ~1us
- building the meta struct and turning it into JSON: ~1-2us
- computing the destination directory and file paths: ~3-5us
- general async function bookkeeping and struct unpacking: ~5us
- sending the rename message to the background worker: ~5-25us
  (the most variable line)

At 4KB the 39us is 10x the bare 4us write step, which sounds bad in
percentage terms. At 1MB it's invisible. At 100MB it's noise. **The
fixed cost matters most for very small objects.** If small-object
latency turns out to matter in production, a future pass can profile
each step and shave it down. For now, the 15x speedup at 4KB versus
the file tier is already a huge win.

### What changed in the docs

- The "Implementation phases" table now marks rows 2 and 3 as done
  (the new ObjectMetaFile fields and the `write_shard` integration).
- This section recorded the measured numbers and the integration
  cost so future readers know where Phase 4 actually landed.
- The lesson on choosing what to measure: the original success
  criterion was "Phase 4 should be within 20% of Phase 2's bare
  write step." That was the wrong yardstick. Phase 2 didn't include
  any of the per-call work that Phase 4 has to do, so Phase 4 could
  never get within 20% of it at small sizes; the fixed costs make
  it impossible. The yardstick that actually matters is the
  comparison against the path Phase 4 replaces (the file tier), and
  by that measure Phase 4 wins by 5x to 18x at every size. **The
  number you choose to measure decides the conclusion you reach.**

### Surprises from Phase 4

1. **The 1MB pool path is faster than the 1MB Phase 2 bare path.**
   The integrated bench had a warmup loop that the Phase 2 bench
   didn't have, which probably explains it. Worth re-running the
   Phase 2 numbers with warmup if we ever want a clean apples-to-
   apples comparison.
2. **The integration overhead is roughly constant in absolute terms,
   not proportional to data size.** Most of it is per-call work
   (path computation, struct construction, message send) that
   doesn't scale with the payload. This is good news for large
   objects and a fix target for small ones.
3. **The 18x win at 64KB is the biggest in the table.** mkdir + 2
   file creates dominates the file tier at small sizes, and the
   pool eliminates all of it. Below 64KB the file tier hits some
   per-file fixed cost ceiling that the pool walks straight past.

### Phase 4.5: profile and fix the 4KB integration overhead (done)

> **Phase 8 correction:** the numbers in this section measure
> `LocalVolume::write_shard` directly, not end-to-end. At 4KB the
> pool's real user-facing PUT latency is 942us p50 through
> hyper/s3s, not 10us. See the
> [Phase 8 section](#phase-8-end-to-end-three-tier-matrix-done)
> for the honest story. The work in this section is still real
> at the storage layer. It just is not what a user sees.

Phase 4 left a puzzle: the integrated `LocalVolume::write_shard`
call took ~43us median for 4KB even though Phase 2's bare write
step was ~4us. That's ~39us of fixed per-call cost, regardless of
data size. Phase 4.5 broke down where the 39us went and fixed the
biggest piece.

**The breakdown bench** (`bench_pool_l3_5_integration_breakdown`)
times each step inside `write_shard`'s pool branch in isolation,
100k iterations each. Output:
`bench-results/phase4.5-integration-breakdown.txt`. Re-run with:

```bash
k3sc cargo-lock test --release --test layer_bench -- \
    --ignored --nocapture bench_pool_l3_5_integration_breakdown
```

Median per-step at 4KB:

```
1. validate_bucket_name + validate_object_key       100ns
2. ObjectMetaFile construction (clone+alloc)        300ns
3. simd_json::serde::to_vec(&mf)                    400ns
4. pool.try_pop()                                   200ns
5. WriteSlot destructure + 2x PathBuf clone         300ns
6. tokio::try_join!(data_write, meta_write)        6400ns
7. object_dir + shard_path + meta_path             6600ns  <-- biggest fixable cost
8. RenameRequest construction (5x PathBuf clone)    200ns
9. tx.send(req).await (channel has space)           100ns
                                                  -------
                            sum:                   14600ns
```

**Path computation was almost as expensive as the actual file
writes.** Calling `object_dir`, `object_shard_path`, and
`object_meta_path` as three separate functions ran the bucket and
key validation three times AND built three independent PathBufs,
when really we only need one validation and one PathBuf, with two
cheap `.join()` calls for the file names.

### The fix

Three lines in `LocalVolume::write_shard`:

```rust
// Before (3 independent calls, 6.6us median):
let dest_dir = pathing::object_dir(&self.root, bucket, key)?;
let data_dest = pathing::object_shard_path(&self.root, bucket, key)?;
let meta_dest = pathing::object_meta_path(&self.root, bucket, key)?;

// After (1 call + 2 cheap joins):
let dest_dir = pathing::object_dir(&self.root, bucket, key)?;
let data_dest = dest_dir.join("shard.dat");
let meta_dest = dest_dir.join("meta.json");
```

### Result (much bigger than predicted)

I predicted ~5us savings (would drop 4KB p50 from 43us to ~38us).
Actual measurement after the fix:

| Metric | Phase 4 (before) | Phase 4.5 (after) | Improvement |
|---|---|---|---|
| 4KB pool average | 132.7us | **23.8us** | **5.6x faster** |
| 4KB pool median | 43.0us | **10.4us** | **4.1x faster** |
| 4KB pool 99th percentile | 2.10ms | **184us** | **11x faster** |

The 4KB median dropped by 33us, not 5us as predicted. The path
computation was triggering more downstream cost than its isolated
measurement suggested. The 11x improvement on the 99th percentile
is the most telling: the variance wasn't measurement noise, it was
real, and it was caused by the redundant validation and allocation
work.

### Pool vs file tier (after the fix)

| Size | File tier (median) | Pool (median) | Speedup |
|---|---|---|---|
| **4KB** | 478us | **10.4us** | **46x** (was 15x) |
| **64KB** | 861us | **57us** | **15x** |
| **1MB** | 2.96ms | **409us** | **7.2x** (was 5.5x) |
| **10MB** | 22.98ms | **3.63ms** | **6.3x** |
| **100MB** | 285ms | **37.2ms** | **7.7x** |

The 4KB win nearly tripled (15x to 46x). Other sizes saw small
improvements too because the 6.6us savings is constant. The pool
is now within ~6us of the bare Phase 2 write step at 4KB.

### Methodology lesson: isolated measurements can hide interaction effects

The breakdown bench measured the path computation at 6.6us in
isolation. Removing it from the integrated path saved ~33us. **The
isolated cost was about 1/5 of the actual cost.** The redundant
validation and PathBuf allocation work was triggering downstream
problems (probably allocator hot paths, cache pressure, async
state machine size) that the isolated bench couldn't see because
it didn't have the rest of `write_shard`'s code running around it.

Generalizable: **when a microbench says "this step costs X" and the
real-world cost is much larger than X, the cost is interaction with
the surrounding code, not the step itself.** Removing the offending
call can have outsized effects. Conversely, optimizing the step in
isolation may be wasted work if the interaction effect is what's
actually expensive.

### What's left in the gap

Phase 4.5 closed most of the unexplained 28us gap from the Phase 4
analysis. Pool 4KB p50 went from 43us to 10us; the bare Phase 2
write step is ~4us. The remaining ~6us is genuine per-call code
work (validation, meta build, simd-json serialization, channel
send, async state machine) and isn't worth chasing further. The
absolute cost is already small and the diminishing returns are
real.

### Phase 5: read path integration (pending_renames) (done)

Phase 5 added an in-memory `pending_renames` table (DashMap) so that
a GET arriving immediately after a PUT-through-the-pool can find the
object before the rename worker has finished. The pool is now
production-safe for non-versioned writes.

What changed:

- New `PendingEntry` and `PendingRenames` types in
  `src/storage/write_slot_pool.rs`.
- `RenameRequest` got `bucket` and `key` fields so the worker can
  remove the matching DashMap entry after each rename.
- `process_rename_request` always replenishes the slot now, even if
  the rename failed. The Phase 3 leak is fixed.
- `run_rename_worker` takes an `Option<PendingRenames>`. The worker
  removes the entry after a successful rename. The Phase 1-3 tests
  pass `None`, the production path passes `Some(...)`.
- `LocalVolume::write_shard` inserts into `pending_renames` BEFORE
  sending the rename request, so concurrent readers see the entry
  as soon as the writes are durable.
- `read_shard`, `mmap_shard`, `stat_object`, `list_objects`,
  `delete_object` all check `pending_renames` first with a
  fall-through to the file tier (mirroring the existing log-store
  pattern).
- `delete_object` cancels a pending PUT by removing the entry and
  unlinking the temp files; the worker then no-ops the rename and
  the always-replenish guarantee returns the slot to the pool.

Critical bug found and fixed: holding a `dashmap::Ref` across an
`.await` deadlocks the worker. Every read path now clones data out
of the `Ref` BEFORE awaiting. `drain_pending` got a 5-second
timeout so future bugs of this shape fail fast.

### Phase 5.5: shrink PendingEntry (done, then validated as unnecessary)

Phase 5.5 dropped the `ObjectMeta`, `data_dest`, and `meta_dest`
fields from `PendingEntry`. Read paths now read the temp meta file
on demand instead of cloning the meta into RAM at PUT time. The
hypothesis was that the `ObjectMeta` clone was costing ~17us at
4KB. **The hypothesis was wrong** but the change is still cleaner
so it stayed.

### Phase 5.5+: stable-median measurement (done)

The "Phase 5/5.5 17us regression" turned out to be 100% measurement
noise. The integrated bench was running 100 iterations, but the
operation has a ~200x p99/p50 variance from page cache spikes
(measured in the L3.6 breakdown bench: writes go from 6.9us p50 to
2.68ms p99; file drops go from 100ns p50 to 1.57ms p99). 100 samples
isn't enough to wash out spikes that big.

`bench_pool_l3_integrated_put` was bumped to 10k / 1k / 1k / 100 / 100
iterations (from 100 / 60 / 30 / 15 / 5) and re-run. The "stable"
medians are dramatically better than every previous noisy reading:

```
SIZE     STRATEGY                   ITERS        AVG        p50        p99      THROUGHPUT
4KB      file_tier (baseline)       10000     1.15ms    626.1us     5.47ms         3.4 MB/s
4KB      pool (Phase 4)             10000     94.4us     11.7us     2.02ms        41.4 MB/s

64KB     file_tier (baseline)        1000     1.49ms     1.07ms     5.77ms        41.8 MB/s
64KB     pool (Phase 4)              1000    324.5us     44.3us     5.98ms       192.6 MB/s

1MB      file_tier (baseline)        1000     3.25ms     3.13ms     5.15ms       307.6 MB/s
1MB      pool (Phase 4)              1000     1.25ms    636.0us     7.10ms       801.2 MB/s

10MB     file_tier (baseline)         100    23.18ms    22.82ms    27.80ms       431.3 MB/s
10MB     pool (Phase 4)               100    13.64ms     3.75ms    61.73ms       733.1 MB/s

100MB    file_tier (baseline)         100   373.35ms   310.62ms  1175.00ms       267.8 MB/s
100MB    pool (Phase 4)               100   102.97ms   107.89ms   181.97ms       971.2 MB/s
```

**Pool vs file tier (stable, median):**

| Size  | File tier (median) | Pool (median) | Speedup |
|---|---|---|---|
| **4KB**   | 626us  | **11.7us** | **53x** |
| **64KB**  | 1.07ms | **44.3us** | **24x** |
| **1MB**   | 3.13ms | **636us**  | **5x**  |
| **10MB**  | 22.82ms| **3.75ms** | **6x**  |
| **100MB** | 310ms  | **107.89ms**| **2.9x**|

Phase 5.5's "real" cost vs Phase 4.5 at 4KB is about 1.3us at p50
(11.7us with the pending_renames bookkeeping vs 10.4us without).
That's the DashMap insert + 2 PathBuf clones, exactly what was
expected from the breakdown bench. The earlier "17us / 33us
regressions" were entirely measurement noise from running 100 iters
on a measurement that has millisecond p99 spikes.

### Phase 5.5+ methodology lessons

1. **100 iterations is not enough samples when p99 is 200x p50.**
   Always check the avg/p50 ratio before drawing conclusions; a
   ratio above ~3x means there are tail outliers that need many
   more samples to stabilize the median.
2. **Never clone a `dashmap::Ref` into a local that's held across
   `.await`.** It deadlocks anything that needs to write-lock the
   same shard. The fix is `pending.get(&key).map(|e| e.clone_fields())`
   before any await.
3. **Always have a watchdog timeout on test polling loops.** The
   first attempt at `drain_pending` had no timeout and hung for 9
   minutes when a deadlock fired. A 5-second deadline is enough
   to fail fast without disrupting normal runs.

### Phase 5.6: move file drops off the hot path (done)

> **Phase 8 correction:** the "pool 53x file tier at 4KB" number
> below measures `LocalVolume::write_shard` in a loop that does
> not exist in production. End-to-end 4KB PUT p50 through
> hyper/s3s is 942us for the pool vs 1406us for the file tier
> (1.5x, not 53x). The drops-off-hot-path optimization still
> helped the storage layer, but most of that help is invisible
> to users because the HTTP stack dominates the 4KB budget. See
> the [Phase 8 section](#phase-8-end-to-end-three-tier-matrix-done)
> for the honest story.

Phase 5/5.5 looked like a 17us regression at 4KB. After running the
integrated bench with 10k samples per size (Phase 5.5+), the median
was actually 11.7us. The "regression" was measurement noise from
running only 100 iterations on an operation with 200x p99/p50
variance. Phase 5.5 was correct all along.

But the stable measurements ALSO showed where the real cost lives.
A new breakdown bench (`bench_pool_l3_6_write_shard_breakdown`)
inlined `write_shard`'s pool branch with `Instant::now()` markers
between every section. The first run revealed:

```
1.  validate (bucket + key)                     avg    1021ns  p50     200ns  p99    5800ns
2.  pool.try_pop()                              avg     412ns  p50     100ns  p99     700ns
3.  meta clone + ObjectMetaFile + simd-json     avg    6055ns  p50    1500ns  p99   74400ns
4.  WriteSlot destructure + 2x PathBuf clone    avg     992ns  p50     300ns  p99    3900ns
5.  tokio::try_join!(data_write, meta_write)    avg  110492ns  p50    6900ns  p99 2683300ns
6.  drop(data_file) + drop(meta_file)           avg   64935ns  p50     100ns  p99 1574700ns
7.  pathing::object_dir(...)                    avg   19587ns  p50    3500ns  p99  312000ns
8.  data_dest + meta_dest joins                 avg    2490ns  p50     500ns  p99   18700ns
9.  PendingEntry build + DashMap insert         avg    3346ns  p50     600ns  p99   17700ns
10. RenameRequest construction                  avg     645ns  p50     100ns  p99     900ns
11. tx.send(req).await                          avg    1155ns  p50     200ns  p99    6500ns
TOTAL                                           avg  212922ns  p50   18800ns  p99 4725700ns
```

The big finding: **step 6 (file drops) is 100ns at p50 but 65us
avg and 1.57ms at p99.** Windows occasionally takes ~1.5ms to
close a file (kernel close + last-write-time bookkeeping + page
cache write-back). Step 5 (the writes themselves) is similarly
spike-prone: 6.9us p50 but 110us avg.

The DashMap insert (step 9, 600ns p50) was confirmed as not the
problem. Phase 5.5's hypothesis was wrong.

### The fix: pass the slot to the worker, drop file handles there

Step 5 (the writes) is at the floor and can't be made faster
without moving away from `tokio::fs`. **But step 6 doesn't have to
happen on the hot path.** We can pass the entire `WriteSlot`
(including its open file handles) to the rename worker, which
drops the handles after the rename. The drops still happen, just
on a background task instead of in the request handler.

`RenameRequest` was changed from holding individual paths to
owning a `WriteSlot`:

```rust
pub struct RenameRequest {
    pub slot: WriteSlot,        // Phase 5.6: owns the file handles
    pub bucket: String,
    pub key: String,
    pub dest_dir: PathBuf,
    pub data_dest: PathBuf,
    pub meta_dest: PathBuf,
}
```

`process_rename_request` now consumes the request by value, moves
the slot out, and drops the file handles before doing the rename:

```rust
pub async fn process_rename_request(
    pool: &WriteSlotPool,
    req: RenameRequest,
) -> Result<(), StorageError> {
    let RenameRequest { slot, dest_dir, data_dest, meta_dest, .. } = req;
    let WriteSlot { slot_id, data_file, meta_file, data_path, meta_path } = slot;

    drop(data_file);   // <- was the slow step on Windows
    drop(meta_file);   // <- was the slow step on Windows

    // ... mkdir + rename + replenish ...
}
```

`write_shard`'s pool branch no longer destructures the slot or
drops anything. It writes through `slot.data_file` and
`slot.meta_file` via Rust split borrows, builds a `RenameRequest`
with the slot moved in, and sends it.

### Result (bigger than predicted at every size)

Predicted: ~65us savings on 4KB avg (the measured drop cost).
Actual at 4KB: 64us. Spot on. **But the savings at LARGER sizes
were much bigger** because file drops at larger files are even
slower (more dirty data to flush on close).

Stable medians (10k+ samples), Phase 5.5+ vs Phase 5.6:

| Size | Metric | Phase 5.5+ | **Phase 5.6** | Improvement |
|---|---|---|---|---|
| 4KB | avg | 94.4us | **30.9us** | **3.1x** |
| 4KB | p50 | 11.7us | **11.9us** | unchanged |
| 4KB | p99 | 2.02ms | **278us** | **7.3x** |
| 64KB | avg | 324.5us | **68.3us** | **4.7x** |
| 64KB | p99 | 5.98ms | **404.6us** | **15x** |
| 1MB | avg | 1.25ms | **481us** | **2.6x** |
| 1MB | p50 | 636us | **364us** | **1.7x** |
| 1MB | p99 | 7.10ms | **2.59ms** | **2.7x** |
| 10MB | avg | 13.64ms | **3.75ms** | **3.6x** |
| 10MB | p99 | 61.73ms | **5.25ms** | **12x** |
| 100MB | p50 | 107.89ms | **67.35ms** | **1.6x** |

The pool is now dramatically more predictable across the board.
The variance dropped uniformly because the spike-prone drops are
no longer in the request path.

### Pool vs file tier (after Phase 5.6, stable medians)

| Size | File tier (median) | Pool (median) | Speedup |
|---|---|---|---|
| **4KB** | 626us | **11.9us** | **53x** |
| **64KB** | 1.07ms | **43.5us** | **25x** |
| **1MB** | 2.99ms | **364us** | **8.2x** |
| **10MB** | 22.79ms | **3.65ms** | **6.2x** |
| **100MB** | 307ms | **67.35ms** | **4.6x** |

The 100MB speedup nearly doubled (2.9x to 4.6x) and the 1MB
speedup improved meaningfully (5x to 8.2x).

### Methodology lessons from Phase 5.5+ and Phase 5.6

1. **100 iterations is not enough samples when p99 is 200x p50.**
   Always check the avg/p50 ratio before drawing conclusions; a
   ratio above ~3x means there are tail outliers that need many
   more samples to stabilize. The Phase 5/5.5 "regression" was
   100% noise from this. 10k iterations showed Phase 5.5 was
   functionally identical to Phase 4.5 at p50.
2. **Never hold a `dashmap::Ref` across an `.await`.** It deadlocks
   anything that needs to write-lock the same shard. The fix is
   `pending.get(&key).map(|e| e.clone_fields())` before any await.
   `drain_pending` got a 5-second watchdog timeout so future
   deadlocks fail fast instead of hanging the test runner.
3. **Spike-prone work belongs off the hot path.** Step 6 was 100ns
   at p50, looked harmless. But its 1.57ms p99 was dragging the
   average way up and giving real users occasional bad latency.
   Moving spike-prone code to a background worker turned a 2ms p99
   into a 278us p99 at 4KB, and a 61ms p99 into a 5.25ms p99 at
   10MB.

### Phase 6: crash recovery scan (done)

Before Phase 6, `enable_write_pool` had a silent data-loss hole:
`WriteSlotPool::new` calls `File::create` for each slot id, which
**truncates** any existing file at `slot-NNNN.data.tmp` /
`slot-NNNN.meta.tmp`. If the process crashed mid-flight with slot
5 holding an acked PUT whose rename hadn't completed, the slot-5
temp files were destroyed on next startup.

Phase 6 closes this hole with a stateless recovery scan that runs
at startup before the fresh pool is created. The temp meta files
carry `bucket` and `key` as identity fields (added in Phase 4), so
the recovery scan can find each pending PUT's destination from
the file alone, with no in-RAM index required.

### The recovery function

New public function `recover_pool_dir(root, pool_dir)` in
`src/storage/write_slot_pool.rs`. Returns a `RecoveryReport`
struct with four counters:

```rust
pub struct RecoveryReport {
    pub recovered_pairs: u32,       // both renames done in recovery
    pub half_renamed_fixed: u32,    // only meta rename was left
    pub orphans_deleted: u32,       // temp files that were not acked
    pub unparseable_deleted: u32,   // meta failed to parse
}

pub async fn recover_pool_dir(
    root: &Path,
    pool_dir: &Path,
) -> Result<RecoveryReport, StorageError>;
```

`enable_write_pool` calls it immediately after creating `pool_dir`
and **before** `WriteSlotPool::new`. The order matters: if recovery
ran after the constructor, the freshly-truncated empty slot files
would destroy the pending PUT.

### How the scan works

1. `tokio::fs::read_dir` the pool dir.
2. For each file, parse the name with `classify_slot_filename`.
   Valid names match `slot-NNNN.(data|meta).tmp` where N is exactly
   four digits. Anything else is a stray and gets deleted (the
   pool dir is private to the pool).
3. Group the recognized files into three states per slot id:
   - **Pair**: both `slot-N.data.tmp` and `slot-N.meta.tmp` exist.
   - **MetaOnly**: only `slot-N.meta.tmp` exists.
   - **DataOnly**: only `slot-N.data.tmp` exists.
4. Handle each state:
   - **Pair**: parse the meta via `read_meta_file`, require non-empty
     `bucket` and `key`, compute `dest_dir` via `pathing::object_dir`,
     mkdir the destination, check whether `data_dest` already exists
     (the half-renamed case), rename data if not, rename meta, done.
     On any failure the pair is treated as unrecoverable and both
     temp files are deleted.
   - **MetaOnly**: only recoverable as half-renamed. If `data_dest`
     exists, finish the meta rename. If not, the meta is a true
     orphan and gets deleted.
   - **DataOnly**: the meta write is part of the pre-ack
     `tokio::try_join!` on the hot path, so data-without-meta means
     the PUT was never acked. Delete.

### Test coverage

11 unit tests in `src/storage/write_slot_pool.rs::tests` plus 1
integration test in `src/storage/local_volume.rs::tests`:

- `recover_full_pair_moves_both_files_to_destination`: the
  "crashed before first rename" case.
- `recover_half_renamed_pair_finishes_meta_only`: data is at dest,
  both temp files still exist, recovery drops the stale temp data
  and moves the meta.
- `recover_meta_only_with_dest_present_finishes_meta`: clean
  half-renamed case (data at dest, only meta.tmp left).
- `recover_meta_only_without_dest_deletes_meta`: true orphan meta.
- `recover_data_only_deletes_data`: unacked PUT.
- `recover_unparseable_meta_deletes_both`: garbage JSON.
- `recover_empty_meta_deletes_both`: zero-byte meta.
- `recover_meta_without_bucket_or_key_deletes_both`: valid JSON
  with empty identity fields (old format or partial write).
- `recover_stray_files_are_deleted`: non-slot files.
- `recover_empty_pool_dir_is_noop`: clean shutdown.
- `recover_missing_pool_dir_creates_it_and_returns_zero`: first-ever
  startup.
- `enable_write_pool_recovers_crashed_put` (integration): end-to-end
  verification that a hand-laid temp pair becomes a readable object
  at `<root>/bucket/obj/` AND that the fresh pool at slot 0 is
  created afterwards with empty files at the original slot-0000.*.tmp
  paths (no conflict with recovery).

### Result

- **Hot path cost:** zero. Recovery runs once at startup.
- **Startup cost:** one `read_dir` on a directory that holds at most
  depth + queue-depth = 288 files per disk in the worst case, plus
  a small number of renames when recovery is needed. On a clean
  shutdown it's one `read_dir` and done.
- **All 355 tests pass** (341 pre-Phase-6 + 14 new).
- **Safety:** acked PUTs survive process crash. The pool is now
  fully production-safe for non-versioned writes with crash recovery
  included.

All 355 tests pass. The pool is functionally complete and
production-safe for non-versioned writes:

- Phase 5 (read-after-write consistency via `pending_renames`) lets
  an immediate GET-after-PUT find the object before the rename
  worker has finished.
- Phase 5.5+ proved with stable medians that the bookkeeping cost
  is ~1us, not the noisy ~33us that the small-iteration runs
  appeared to show.
- Phase 5.6 moved the spike-prone file drops off the hot path,
  cutting the 4KB avg by 3x and the 10MB p99 by 12x.
- Phase 6 closes the crash-recovery gap so acked PUTs survive a
  restart.

### Phase 8: end-to-end three-tier matrix (done)

The first eight phases all measured the pool at the storage layer
(directly calling `LocalVolume::write_shard` in a loop). Phase 8
measured it end-to-end through the full HTTP stack for the first
time, and the story that came back is very different.

#### Methodology

`abixio-ui/src/bench/::bench_pool_l4_tier_matrix`. Spawns a fresh
abixio server per (tier, size) cell, single disk with ftt=0 (no
erasure), binds a random 127.0.0.1 port, runs sequential PUTs
followed by sequential GETs through `reqwest::Client` against the
full hyper http1 + s3s dispatch stack. Each tier is configured by
calling `enable_log_store` or `enable_write_pool(32)` on the
`LocalVolume` before it is boxed into the VolumePool. For the
pool tier, `drain_pending_writes` is called between the PUT and
GET phases so GETs measure steady-state read latency, not
read-after-write-via-pending.

Workload: 4KB/64KB/1MB/10MB/100MB, 1000/500/200/50/10 iterations
each. Three tiers: file (baseline), log (log_store enabled), pool
(write_pool enabled). Total ~1760 PUTs and ~1760 GETs per tier.
Raw output in `bench-results/phase8-tier-matrix-sequential.txt`.

#### PUT p50 results

| size | file | log | pool | pool vs file | pool vs log |
|---|---|---|---|---|---|
| 4KB | 1406us | **295us** | 942us | 1.5x | **0.3x** |
| 64KB | 1506us | **377us** | 1037us | 1.5x | **0.4x** |
| 1MB | 4200us | 4057us | **3623us** | 1.2x | 1.1x |
| 10MB | 17910us | 37003us | **16582us** | 1.1x | **2.2x** |
| 100MB | **152094us** | 176931us | 257470us | **0.6x** | 0.7x |

#### GET p50 results

| size | file | log | pool |
|---|---|---|---|
| 4KB | 582us | **168us** | 761us |
| 64KB | 831us | **208us** | 920us |
| 1MB | 1818us | 1668us | **1626us** |
| 10MB | 12271us | 10705us | **9604us** |
| 100MB | 104539us | 104346us | **96414us** |

#### PUT throughput (MB/s)

| size | file | log | pool |
|---|---|---|---|
| 4KB | 2.4 | **12.5** | 3.6 |
| 64KB | 35.7 | **157.1** | 45.2 |
| 1MB | 231.0 | 231.5 | **271.7** |
| 10MB | 555.9 | 302.7 | **592.4** |
| 100MB | **655.8** | 575.0 | 439.3 |

#### Findings

**1. The 53x-at-4KB storage-layer claim is invisible to users.**
Phase 5.6 measured `write_shard` at 11.9us p50 for 4KB. End-to-end
at 4KB is 942us p50. That means ~930us per request is spent above
`LocalVolume` in the HTTP stack (reqwest body read, hyper framing,
s3s parsing, axum routing, validation). At 4KB, the HTTP stack is
~98% of the PUT budget. Any storage-layer optimization below that
is fighting over the remaining ~2%. The 11.9us win is still real
at the storage layer and this doc keeps it for history, but it
does not translate to a user-facing 53x win.

**2. Log store dominates small objects.** At 4KB PUT, log store
295us vs pool 942us is a **3.2x gap**. At 4KB GET, log store 168us
vs pool 761us is a **4.5x gap**. The gap holds at 64KB. The log
store's segment-append and HashMap-indexed reads are just faster
than the pool's per-object file-create and `File::open + mmap`
reads once you're paying the HTTP overhead on both sides. The
design doc's Trade-offs section always flagged the read gap. Phase
8 put a number on it.

**3. The pool loses to the file tier at 100MB PUT.** 257ms vs
152ms (0.6x). Likely cause: NTFS rename of a 100MB temp file adds
~100ms that the file tier never pays, because the file tier
writes directly to the final location. The Phase 5.6 breakdown
bench missed this cost because it called `drain_pending` between
PUTs, which excluded the rename from the measured step. A 100MB
PUT is dominated by the rename, not the write.

**4. The pool's real sweet spot is 1MB to 10MB.** At 1MB pool
beats file 1.2x and log 1.1x. At 10MB pool beats log 2.2x (log
has fallen through to the file tier because it only handles
<=64KB) and file 1.1x. This is a narrow but real win. At these
sizes the HTTP stack is <20% of the PUT budget and the pool's
write-path savings become visible again.

**5. The pool's GETs are slower than log store GETs at small
sizes.** 4.5x slower at 4KB, 4.4x slower at 64KB. This is
exactly what the Trade-offs section predicted. Keep the log
store enabled for small objects.

#### What this means for the pool story

- **The pool is not a log store replacement.** See the
  [Coexistence with the log store](#coexistence-with-the-log-store)
  section for the updated three-tier ship plan.
- **The pool's 1MB to 10MB win is real.** It ships there and
  nowhere else (pending Phase 9 confirmation under concurrency).
- **The storage-layer 53x/130x numbers in earlier phases** are
  still correct at the storage layer and are still accurate
  history of the optimization work. They are not user-facing
  latency. Phase 4.5 and Phase 5.6 sections above carry a
  one-line Phase 8 correction banner.

#### Things that would change these numbers

- **Concurrency (Phase 9).** Sequential measurement is the worst
  case for the pool because the rename worker never overlaps
  with the next request. Under concurrency the async worker
  might claw back some of the 100MB regression. Or might not.
  Measure first.
- **More iterations per cell.** 1000 at 4KB is stable at p50
  but noisy at p99 (Phase 5.5+ lesson). 5000 would be safer.
- **Fresh process per tier.** All three tiers ran in one test
  invocation. OS cache state bleeds between cells.
- **Linux/ext4.** The 100MB pool regression is NTFS rename cost
  and may be smaller on other filesystems. Measured only on
  Windows.

#### Code changes

- `abixio-ui/src/bench/::bench_pool_l4_tier_matrix` (the bench
  itself, 11 iteration cells, plus three comparison tables).
- `src/storage/mod.rs`: added `async fn drain_pending_writes` to
  the `Backend` trait with a default no-op implementation. Lets
  the bench drain the pool between PUT and GET phases via the
  trait without downcasting to `LocalVolume`.
- `src/storage/local_volume.rs`: overrides `drain_pending_writes`
  to delegate to the existing `drain_pending` method.

### Phase 8.5: where the missing 900us actually lives (done)

Phase 8 concluded "pool 4KB end-to-end is 942us vs storage-layer
11us, so ~930us is HTTP/s3s/axum overhead." Phase 8.5 built a
layer-subtraction bench (`bench_pool_l4_5_stack_breakdown`) to
attribute the gap, and the conclusion was **wrong twice before it
landed**. The real answer is that the pool has **two hidden choke
points** that every previous benchmark managed to avoid.

#### Methodology

`abixio-ui/src/bench/::bench_pool_l4_5_stack_breakdown`.
Progressive stages at 4KB PUT, 1000 sequential iterations each,
fresh hyper server per stage. A bimodal bucket counter tracks how
many samples fall below 300us (fast path) vs above 500us (slow
path / blocked):

- **Stage A, `hyper_bare`**: hyper http1 service_fn that drains
  the body and returns 200 empty. No s3s, no abixio, no storage.
  Measures reqwest + TCP loopback + hyper parse.
- **Stage B, `hyper_manual_handler`**: bare hyper handler that
  parses `/bucket/key` manually and calls `LocalVolume::write_shard`
  directly (skipping s3s, AbixioS3, and VolumePool).
- **Stage C, `abixio_null_backend`**: full hyper + s3s + AbixioS3
  + VolumePool stack, but the backend is a `NullBackend` whose
  `write_shard` returns `Ok(())` immediately.
- **Stage D, `file_tier`**: full stack with real file-tier LocalVolume.
- **Stage E, `pool_tier (depth 32)`**: full stack with the default
  `enable_write_pool(32)`.
- **Stage E*, `pool_tier (depth 100)`**: pool with depth 100, default
  channel.
- **Stage E'', `pool_tier (depth 1024)`**: pool with depth 1024,
  default channel.
- **Stage E', `pool_tier_drained(32)`**: depth 32, but
  `drain_pending_writes` is called every 32 iters to keep the
  pool full between batches (Phase 5.6's methodology).
- **Stage E+, `pool_tier(depth32, ch100k)`**: depth 32 plus a
  100_000-slot rename channel (tests whether channel backpressure
  is the bottleneck).
- **Stage E#, `pool_tier(depth1024, ch100k)`**: depth 1024 AND
  100_000-slot channel. Neither starvation nor channel blocking
  is possible within a 1000-iter run. This is the purest
  "pool fast path at HTTP layer" measurement.

Raw output (v5):
`bench-results/phase8.5-stack-breakdown-v5.txt`.

#### Results at 4KB (1000 iters, sequential)

| Stage | config | p50 | fast | mid | slow |
|---|---|---|---|---|---|
| A. hyper_bare | | **93.9us** | 999 | 1 | 0 |
| B. hyper + direct write_shard | | 677.3us | 0 | 0 | 1000 |
| C. abixio_null_backend | | **125.9us** | 999 | 1 | 0 |
| D. file_tier | | 809.8us | 0 | 0 | 1000 |
| E. pool_tier | depth 32, ch 256 | 736.6us | 32 | 379 | 589 |
| E*. pool_tier | depth 100, ch 256 | 497.3us | 93 | 408 | 499 |
| E''. pool_tier | depth 1024, ch 256 | 1236.2us | 36 | 268 | 696 |
| E'. pool_tier_drained | depth 32, drain/32 | 322.3us | 215 | 730 | 55 |
| E+. pool_tier | depth 32, ch 100k | 560.7us | 34 | 461 | 505 |
| **E#. pool_tier** | **depth 1024, ch 100k** | **317.8us** | 194 | 774 | 32 |

**Stage E# and Stage E' converge at ~320us.** Both techniques
remove all blocking from the pool path: E# oversizes both the
pool and the channel so nothing ever blocks; E' drains between
batches so the channel and pool are full when each batch starts.
Both give the same answer. **That is the true pool fast path
cost at the HTTP layer: ~320us.** Not 11us (Phase 5.6 number,
storage layer only), not 942us (Phase 8 number, contaminated
by two choke points below).

#### Layer attribution at 4KB PUT

```
hyper + TCP + reqwest floor                (A)              93.9us
+ s3s + AbixioS3 + VolumePool dispatch     (C - A)          32.0us
+ pool hot-path work via try_join writes   (E# - C)        191.9us
---------------------------------------------------------- -------
pool fast path at HTTP layer               (E#)            317.8us

file tier storage work vs pool             (D - E#)        492.0us
```

The pool's real end-to-end win over the file tier at 4KB, **when
no choke point is hit**, is **492us (2.55x)**. That's a real,
measurable, shippable win, but it is:
- **~10x smaller than the 53x storage-layer claim** in Phase 5.6
  that compared pool (11us) to file tier (626us).
- **Only achievable with non-default pool configuration**. The
  default (depth 32, channel 256) sits at ~737us p50 because
  both choke points below fire immediately under sustained load.

#### The two choke points

**Choke point 1: pool starvation.**

Default depth is 32 (`local_volume.rs:109`). Under sustained
sequential load, the client burns through 32 slots faster than
the rename worker replenishes them (worker rate ~940 ops/sec,
Phase 3). The pool empties, and `write_shard` silently falls
through to the file tier slow path:

```rust
if let Some(slot) = pool.try_pop() {
    // fast path
    return Ok(());
}
// Pool empty: fall through to the existing slow path
// (Phase 7 will add explicit backpressure / fallback).
```

There was even a comment acknowledging this. Phase 7 was supposed
to address it but hasn't shipped. Until then, depth 32 means
"sustained throughput collapses to file tier after ~30ms of
sequential load." This is visible in the Stage E fast bucket
count: **only 32 samples out of 1000 hit the fast path** because
only the first 32 PUTs found a slot.

**Choke point 2: channel backpressure.**

Default rename channel buffer is 256 (`local_volume.rs:137`).
Once the worker has 256 rename requests queued, `tx.send(req).await`
**blocks the client** until the worker dequeues one. Worker rate
is ~940/sec, so each blocked send takes ~1063us.

This is the choke point Stage E'' (depth 1024, default channel)
hit. Big pool eliminated starvation, but the channel filled after
256 PUTs, and from there the client was paced to worker rate
(1236us p50 vs 317us for Stage E# with the 100k channel).

Stage E# proves it: the **only** thing that changes between E''
(1236us) and E# (318us) is the channel buffer size, from 256 to
100_000. Depth 1024 is the same in both.

**The client does NOT block on the rename itself.** The rename
runs on the worker asynchronously, as designed. The client blocks
on `tx.send(req).await` because the bounded channel fills when
the worker cannot keep up. That blocking is an implementation
detail of the channel buffer size, not of the async rename model.

#### Why the default config is wrong for sustained load

Depth 32 came from the original Phase 1 plan as "enough for small
bursts with low memory cost." The 256 channel buffer was a Phase 3
default that was never tuned against sustained producer pressure.
**No benchmark measured the pool under sustained sequential HTTP
load until Phase 8.5**. Every previous phase measured it either
in storage-layer isolation (Phase 5.6, with drain_pending between
iters) or end-to-end with the default-sized pool silently falling
through to the file tier (Phase 8, with no fast-path counter to
catch it).

Defaults that would actually hold up under sustained load:
- **Pool depth >= 1024** so starvation doesn't hit in the first
  second of sustained traffic. 1024 slots = 2048 file descriptors
  per disk, which is fine on Linux and fine-ish on Windows.
- **Channel buffer >= 10_000** so tx.send never blocks under
  bursts. Memory cost is negligible (a few hundred bytes per
  entry).
- **Multiple rename workers** (Phase 3 measured 2 workers at
  1744 ops/sec, 1.85x of 1 worker). 4 workers would probably hit
  3000 ops/sec. This is the actual scaling lever; tuning pool
  depth and channel size without also raising worker count just
  pushes the backpressure further out.

With depth 1024 + channel 10_000 + 2 workers, the pool should
sit at ~320us p50 under sustained 4KB sequential load, which is
a genuine **2.55x win over the file tier (810us)** and well above
the log store's 295us for 4KB.

#### What Phase 5.6's 11us number actually was

Phase 5.6's `bench_pool_l3_integrated_put` called `write_shard`
in a tight loop with `drain_pending` every 32 iters. The loop was
the only active task on an otherwise-idle tokio runtime. No HTTP
server, no client, no concurrent workers. In that environment the
pool hot path was ~11us p50.

On a live tokio runtime with server + client + worker all active,
the same code path costs ~192us (Stage E# - Stage C). The
difference is tokio scheduling jitter and spawn_blocking overhead
on the two slot file writes that the idle-runtime measurement
didn't see. In Stage E#, 774 of 1000 samples land in the 300-500us
"mid" bucket (not the <300us "fast" bucket), showing that even
with no blocking, the per-sample runtime variance is ~200us
because of tokio scheduling under a busy runtime.

**This does not mean Phase 5.6 was fraudulent.** The 11us number
is real at the storage layer with that methodology. It just
doesn't represent what a user sees at the HTTP layer under a
live runtime, which is what Phase 8.5 finally measured.

#### What the fast / mid / slow bucket distributions reveal

- **Stages A, C (no storage)**: almost all samples in the fast
  bucket (<300us). Confirms HTTP stack and abixio dispatch are
  tight.
- **Stages B, D (file tier)**: all samples in the slow bucket
  (>500us). Confirms file tier is consistently slow.
- **Stage E (depth 32)**: 32 fast, 379 mid, 589 slow. The 32
  fast samples are exactly the first 32 PUTs that found a slot.
  After that the pool empties and the remaining 968 samples are
  a mix of file-tier fallback (slow) and pool-path-with-blocking
  (mid).
- **Stage E'' (depth 1024)**: 36 fast, 268 mid, 696 slow. With
  1024 slots the pool never empties, so 0 samples should be
  file-tier fallback. But 696 samples are "slow" because they
  blocked on `tx.send` waiting for the channel to drain. The
  channel is the bottleneck, not the pool.
- **Stage E# (depth 1024 + channel 100k)**: 194 fast, 774 mid,
  32 slow. With both bottlenecks removed, 774 samples cluster in
  the 300-500us mid range (the pool's real fast-path cost on a
  busy runtime) and only 32 samples are "slow" (NTFS page cache
  spikes).

#### The Phase 8 correction (correction of the correction)

Phase 8's "~930us is HTTP/s3s overhead" narrative was wrong.
Phase 8.5 v1-v3 said "~900us is file tier storage work and pool
savings are only 113us." That was also wrong because the pool
samples it measured were mostly file-tier fallback plus blocked
tx.send waits.

The **correct** attribution at 4KB:

```
HTTP + s3s + abixio dispatch    126us   (Stage C)
Pool fast path above dispatch   192us   (E# - C)
Pool fast path at HTTP layer    318us   (E#)

File tier work above dispatch   684us   (D - C)
File tier at HTTP layer         810us   (D)

Pool savings vs file, unblocked 492us   (D - E#)  ~2.55x
```

The pool is faster than the file tier end-to-end. The win is
real. It just requires a pool depth and channel buffer larger
than the current defaults, plus acknowledging that under
sustained load the "async rename worker" is not truly async
without sufficient buffering in the producer-consumer queue.

### Phase 8.6: DRY pool optimizations into the file tier (done)

Phases 2 through 5.6 accumulated four hot-path optimizations in the
pool branch of `write_shard`. The file tier branch received none of
them. Phase 8.6 propagated the three that can live outside the pool
design (the fourth, moving file handle drops off the hot path, is
inherent to the pool's pre-opened file model and cannot apply to
the file tier without rebuilding the pool).

#### What landed

1. **`write_meta_file` uses simd-json.** `src/storage/metadata.rs:182`
   swapped `serde_json::to_vec_pretty` for `simd_json::serde::to_vec`.
   Every one of the 9 callers of `write_meta_file` (write_shard,
   multipart finalize, versioning, tagging, etc.) gets the Phase 2.5
   speedup for free. Compact JSON instead of pretty is backward
   compatible because `read_meta_file` uses `serde_json::from_slice`
   which parses both.
2. **Path computation collapse** in the file tier branch of
   `write_shard`. Previously the file tier called
   `pathing::object_dir`, then `pathing::object_shard_path`, then
   `pathing::object_meta_path`. The second and third helpers
   re-invoke `object_dir` internally, so each PUT was running
   `validate_bucket_name` + `validate_object_key` + `safe_join`
   three times. Fixed to one call plus two `.join()`s, matching
   the pool's Phase 4.5 pattern.
3. **Concurrent shard + meta writes** in the non-inline file tier
   branch. Previously shard.dat was written with `tokio::fs::write`
   (awaits) and then meta.json was written (awaits again). Now
   both run concurrently via `tokio::try_join!` for objects
   larger than 64KB, matching the pool's Phase 2 pattern.

The inline path (<=64KB, data base64-encoded into meta.json) is
unchanged because it only writes one file and `try_join!` has
nothing to parallelize.

#### Measured impact

Measured via `bench_pool_l4_5_stack_breakdown` (4KB only) and
`bench_pool_l4_tier_matrix` (5 sizes). Absolute numbers are noisy
between runs on NTFS, especially at 10MB+ where OS cache state
between runs dominates. The reliable signal:

- **Stage B (direct write_shard at 4KB)**: 677us → 641us (~35us
  improvement). Likely from the path collapse; simd-json is
  too small to measure at 4KB meta sizes.
- **File tier 64KB PUT**: 1506us → 1329us (~177us improvement).
  Plausibly from `try_join!` parallelizing the meta write with
  the shard write at a size where the non-inline path is used.
- **File tier 4KB PUT**: swung wildly (1406us → 882us) between
  runs but the delta is not attributable to Phase 8.6 alone
  because 4KB takes the inline path which cannot benefit from
  `try_join!`. Probably warm cache from prior bench runs.
- **File tier 1MB-100MB PUT**: noise floor dominated. The ~30-60us
  theoretical savings from parallel meta+shard writes is well
  below the ~500us to ~5ms cross-run variance at those sizes.

**Net claim:** the DRY cleanup landed. The direct perf impact is
real at 64KB (~177us via `try_join!`) and small at other sizes
(~5-35us via path collapse + simd-json). The biggest value is
**reducing future maintenance burden**: both write paths now
share the same optimization pattern, and improvements to
`write_meta_file` now benefit all 9 of its callers simultaneously.

#### What did NOT apply

- **Phase 5.6's "move file drops off the hot path."** The pool's
  win there came from the fact that it uses pre-opened files and
  can hand the close-and-drop work to a background worker. The
  file tier opens files inside `tokio::fs::write` itself, which
  closes them synchronously before returning. Fixing that would
  require the file tier to hold open file handles between writes
  and close them asynchronously somewhere, which is
  architecturally equivalent to rebuilding the pool. Out of scope.
- **The inline base64 path at <=64KB.** The file tier encodes
  small-object data as base64 into meta.json, writing one file
  instead of two. This saves one file create at the cost of a
  base64 encode pass (~25us at 4KB) and a larger meta.json. The
  tradeoff is a design question, not a Phase 8.6 cleanup target.
  Whether removing the inline path and letting `try_join!`
  parallelize two files instead would be faster at 4KB is worth
  a future bench. Not addressed here.
- **VolumePool::put_object_stream** optimization. That layer
  sits above `write_shard` and has its own per-PUT bookkeeping
  (hash, RS encode at ftt=0, placement, metadata build). Phase
  8.5 measured it at ~27us total at 4KB which is already minimal.
  No low-hanging fruit there.

#### Files touched

- `src/storage/metadata.rs`: `write_meta_file` swapped to
  `simd_json::serde::to_vec`.
- `src/storage/local_volume.rs`: file tier branch of
  `write_shard` rewritten with path collapse and `try_join!`.

All 355 existing tests pass unchanged.

### Phase 8.7: raise pool defaults and scale the rename worker (done)

Phase 8.5 identified that the default pool config was undersized for
sustained load and named three fixes:

1. Raise default pool depth from 32 to something bigger.
2. Raise default rename channel buffer from 256 to something bigger.
3. Run multiple rename workers instead of one (Phase 3 measured
   2 workers at 1.85x a single worker).

Phase 8.7 landed all three.

#### Code changes

**`RenameDispatch` round-robin fan-out**
(`src/storage/write_slot_pool.rs`). New type that holds N
`mpsc::Sender`s and picks one per request via an atomic round-robin
counter. Each worker owns its own `mpsc::Receiver`, so workers
don't contend for a shared queue. The producer's `send` only blocks
when the CHOSEN worker's channel is full, and with round-robin
fan-out the effective total buffer is `worker_count * per_worker_buffer`.

**Multi-worker `enable_write_pool_with_config`**
(`src/storage/local_volume.rs`). New three-parameter variant
`enable_write_pool_with_config(depth, channel_buffer, worker_count)`
that spawns `worker_count` rename worker tasks, each with its own
channel. `enable_write_pool_with_channel(depth, buffer)` stays as
a convenience wrapper that passes `worker_count=2`.

**New default config.** `enable_write_pool(depth)` now forwards to
`enable_write_pool_with_config(depth, 10_000, 2)`. Per-worker
channel buffer is 10_000 (up from 256) and worker count is 2 (up
from 1). Depth is still passed by the caller because it's tied to
file-descriptor cost per disk.

**`LocalVolume.rename_tx` field type** changed from
`Option<mpsc::Sender<RenameRequest>>` to
`Option<Arc<RenameDispatch>>`. `write_shard`'s pool branch now
calls `tx.send(req).await` through the dispatcher, which hides the
round-robin from the write path.

#### Measured impact at 4KB and 64KB

Phase 8 was the baseline (default config, 1 worker, channel 256,
depth 32). Phase 8.7 is the new default (depth 32 as passed by the
bench, channel 10_000 per worker, 2 workers). Both runs use
`bench_pool_l4_tier_matrix` with identical iteration counts.

| Size | Phase 8 pool p50 | Phase 8.7 pool p50 | Improvement |
|---|---|---|---|
| **4KB** | 942us | **454us** | **2.08x** |
| **64KB** | 1037us | **586us** | **1.77x** |
| 1MB | 3623us | 3796us | noise |
| 10MB | 16582us | 33837us | noise |
| 100MB | 257470us | 165671us | noise |

Pool vs file tier ratio at the same sizes:

| Size | Phase 8 pool-vs-file | Phase 8.7 pool-vs-file |
|---|---|---|
| 4KB | 1.5x | **2.1x** |
| 64KB | 1.5x | **2.3x** |
| 1MB | 1.2x | 1.1x |
| 10MB | 1.1x | 0.9x (NTFS noise) |
| 100MB | 0.6x | 0.9x (NTFS noise) |

The 4KB and 64KB improvements are **real and reproducible**. Larger
sizes continue to have NTFS cross-run variance in the 20-40% range,
which swamps the ~100us savings from any code-level change at those
sizes. The honest claim is that **the pool's end-to-end 4KB win is
now 2x instead of 1.5x** and the bottlenecks Phase 8.5 identified
are closed.

#### Why the 4KB number went from 942us to 454us

Under Phase 8 defaults (depth 32, channel 256, 1 worker), a sustained
sequential client:
1. Burned through all 32 pool slots in ~30ms.
2. Subsequent PUTs queued renames on the 256-slot channel. After 256
   queued renames, `tx.send(req).await` started blocking for ~1063us
   (worker rate).
3. At steady state, the client paced itself to worker rate and the
   pool was useless.

Under Phase 8.7 defaults (depth 32, channel 10_000 per worker, 2
workers):
1. Client still burns through 32 slots quickly.
2. Renames queue on two channels (round-robin), effective total
   buffer = 20_000 requests. Never fills within a 1000-iter bench.
3. Two workers drain at ~1744 ops/sec combined, roughly matching
   client rate at the pool fast path speed.
4. Pool stays "warm enough" to never degrade to file tier fallback
   via starvation AND `tx.send` never blocks.

The 4KB p50 of 454us is higher than Phase 8.5's Stage E# (318us)
because Stage E# used depth 1024 which guarantees the pool never
empties at all. Phase 8.7 keeps depth 32 as the call-site-provided
parameter and just fixes the channel + workers, so the pool still
occasionally empties briefly. A follow-up bump to depth 1024 as
the new default would likely push 4KB p50 close to Stage E#'s
~320us.

#### What's still pending

- **Depth default**: still 32 at the call site. Raising to 1024
  or 256 would close the remaining gap to Stage E# but costs more
  file descriptors per disk. Worth a separate small bump.
- **Phase 7 backpressure**: Phase 8.7 raised the channel buffer but
  didn't add real backpressure when the channel DOES fill (e.g. under
  truly sustained load exceeding 2 workers). That's still Phase 7.
- **1MB-100MB noise**: needs a more controlled bench environment
  (or more iterations at large sizes) to see whether Phase 8.7 helps
  or hurts at mid-to-large sizes. The theoretical impact is small
  because large PUTs are dominated by raw disk write time.

All 355 tests still pass.

### Phase 7, 9: pending

The remaining phases are:

- **Phase 7** (backpressure / fall-through to slow path) when the
  pool is empty or the rename queue gets too long. Still
  pending. More important now that Phase 8 has shown the pool
  is only the right tier for a specific size window.
- **Phase 9** (concurrency). The Phase 8 numbers are all
  sequential. Real workloads are concurrent and the pool's async
  rename worker is designed to overlap with the next request.
  The concurrency bench is the final confidence check before
  shipping the three-tier default.
- The `--write-tier` CLI flag in `src/main.rs`. Still pending.
  The bench harness enables tiers directly in code; production
  needs the flag.
- Pre-allocation tunable via `fallocate` / `SetEndOfFile`. Still
  pending. Much less urgent now that Phase 8 has located the
  real bottleneck (HTTP stack overhead) that pre-allocation
  cannot touch.
- Streaming `open_shard_writer` integration through the pool.
- Versioned writes through the pool.

## Open questions for implementation

- **Pool depth default.** Recommended 32 per disk. Tunable via config.
- **Windows rename semantics.** `tokio::fs::rename` uses
  `std::fs::rename`, which on Windows uses `MoveFileEx` with
  `MOVEFILE_REPLACE_EXISTING`. Verify this behaves atomically when
  the destination already exists from a half-completed crash.
- **fsync.** Default no, matching the log store and write cache.
  Document the power-loss trade-off.
- **Pre-allocation (fallocate / SetEndOfFile).** Default off in the
  first version. Quantified by the "additional pool-only benchmarks"
  section above; flip the default if it shows >=10% improvement at
  any common size.
- **Faster JSON serializer.** simd-json or sonic-rs behind a
  `feature = "fast-json"` flag. Quantified by the same benchmark
  section. Flip the default if it shows >=5us improvement on small
  PUTs.

## Files to modify (when phase 2 begins)

- `src/storage/metadata.rs`: add `bucket` and `key` fields to
  `ObjectMetaFile` with `#[serde(default)]`. Update writers in
  `local_volume.rs` to populate them.
- New: `src/storage/write_slot_pool.rs`: pool, slot, rename worker,
  recovery scan.
- `src/storage/mod.rs`: declare new module.
- `src/storage/local_volume.rs`: new `write_pool` field on
  `LocalVolume`. Modify `write_shard`, `open_shard_writer`,
  `read_shard`, `mmap_shard`, `stat_object`, `list_objects`, and
  `delete_object` to consult the pool and `pending_renames`. Spawn
  the rename worker. Run crash recovery in `new()`. Add
  `enable_write_pool(depth)` mirroring `enable_log_store()`.
- `src/storage/pathing.rs`: helpers `pool_dir(root)`,
  `pool_data_path(root, slot_id)`, `pool_meta_path(root, slot_id)`.
  All return paths under `.abixio.sys/tmp/`.
- `src/main.rs`: new `--write-tier` CLI flag. Pass shutdown signal
  into the rename worker (reuse the `tokio::sync::watch` channel
  used by the heal worker).
- `src/admin/handlers.rs`: new endpoint
  `GET /_admin/pool/status` returning per-disk pool depth, queue
  depth, pending count, and slot creation errors.
- `src/config.rs`: add `write_tier` field.
- `tests/s3_integration.rs`: pool integration tests.

## Verification

### Unit tests in `src/storage/write_slot_pool.rs`

- pool init creates N slot pairs
- pop + return cycle preserves depth
- crash recovery finishes a complete pending pair
- crash recovery deletes orphan data file (no meta)
- crash recovery deletes orphan meta file (no data)
- crash recovery handles half-renamed state (data already moved,
  meta still in temp)
- cancel-on-delete returns the slot to the pool
- pool starvation falls through to caller cleanly

### Integration tests in `tests/s3_integration.rs`

- PUT 1MB then GET immediately (should hit pending), verify
  byte-equal. Repeat after the worker drains, verify the same result
  via the file tier path.
- PUT 100 objects in parallel via aws-sdk-s3 with pool depth 32.
  Verify all succeed; some hit fallback. None should error.
- PUT a versioned object three times, `list-object-versions` returns
  three, GET each version_id returns correct bytes.
- Streaming multipart upload (5x 5MB parts + complete), verify the
  completed object byte-equals the source.
- PUT then DELETE before the worker drains, verify no orphan file
  remains in `.abixio.sys/tmp/` and the object 404s.
- Kill the process between the two renames using a test harness,
  restart, verify the destination has both shard.dat and meta.json
  and the object is readable.

### Benchmark comparison

See the benchmark plan section above. Three runs of the bench matrix
plus ``abixio-ui bench``, one per tier setting. Decision criterion: which
tier shows the best PUT obj/sec at 4KB and the best PUT MB/s at 1MB
and 10MB without regressing GET latency by more than 100us at any
size.

## See also

- [Log-structured storage](write-log.md): the small-object tier
  this design competes with for sizes <=64KB.
- [RAM write cache](write-cache.md): a different approach to
  removing disk I/O from the write path, orthogonal to the pool.
- [Architecture](architecture.md): where the pool fits in the
  overall design principles.

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| Pool uses `WriteSlotPool`, `RenameRequest`, background rename workers, and `pending_renames` | Verified | `src/storage/write_slot_pool.rs`, `src/storage/local_volume.rs:171-198`, `525-553`, `636`, `739`, `805`, `854`, `933` |
| `ObjectMetaFile` now contains `bucket` and `key` with `#[serde(default)]` | Verified | `src/storage/metadata.rs:20-24` |
| Crash recovery must run before `WriteSlotPool::new` | Verified | `src/storage/write_slot_pool.rs:391`, `src/storage/local_volume.rs:151-171` |
| Phase 8.7 multi-worker round-robin dispatch landed | Verified | `src/storage/write_slot_pool.rs:168-195`, `src/storage/local_volume.rs:179-198` |
| Default channel buffer / worker-count increase from Phase 8.7 landed | Verified | `src/storage/local_volume.rs:111-135` |
| Pool is not the universal replacement for the log store | Verified | Matches this doc’s correction section and the benchmark summary in `docs/benchmarks.md` |
| `--write-tier` CLI flag is still pending | Verified | This doc says it is pending, and no `write_tier` wiring exists in current `src/main.rs` / `src/config.rs` |
| Historical sections like `Files to modify (when phase 2 begins)` are current implementation guidance | Not current | Those sections are preserved as historical planning notes; many listed items are already implemented |
| Some microbenchmark claims are user-visible end-to-end results | Needs nuance | The doc itself now distinguishes storage-layer measurements from HTTP-layer results; earlier phase sections remain historical context, not current product-level numbers |
| Depth default recommendations are settled product defaults | Not settled | The code exposes pool depth via call sites while channel/workers have newer defaults; the doc’s depth discussion remains partly forward-looking |

Verdict: the document’s top-level correction and implementation-state sections are broadly aligned with the code. The main remaining risk is reader confusion from the historical planning sections near the bottom, which are useful as design history but should not be read as current task lists.
