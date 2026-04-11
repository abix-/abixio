# Pre-opened temp file pool

A write-path mechanism for the file tier. Each disk holds a small pool
of already-open temp files. A PUT writes shard bytes to one slot's
data file and meta JSON to its companion meta file, then acks. The
mkdir, file creates, and the rename to the final destination all
happen on a background worker after the client has been told the PUT
succeeded. Two syscalls on the hot path instead of seven.

This is a sibling to [the log store](write-log.md) and the
[RAM write cache](write-cache.md). Each tier optimizes a different
write pattern. The pool exists to extend log-store-class write speed
to objects that the log store doesn't handle today (>64KB, versioned,
streaming).

## Why

Today's file tier (objects > 64KB, all versioned writes, the streaming
`LocalShardWriter` path) does this on every PUT, per disk:

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
always-open segment. The pool extends the same insight to arbitrary
sizes: keep some files open in advance, just write to them on PUT,
move them to their final location later.

## How it works

### Per-disk slot pool

On `LocalVolume::new`, each disk creates a pool of N pre-opened slots
in `.abixio.tmp/preopen/`. Default N is 32. Each slot is a pair of
files:

```
.abixio.tmp/preopen/
  slot-0000.data.tmp     <- pre-opened, empty, will receive shard bytes
  slot-0000.meta.tmp     <- pre-opened, empty, will receive meta JSON
  slot-0001.data.tmp
  slot-0001.meta.tmp
  ...
  slot-NNNN.data.tmp
  slot-NNNN.meta.tmp
```

The pool holds N `WriteSlot` values, each carrying both file handles
and both paths. A consumer pops one, fills both files, and sends a
rename request to the per-disk async worker.

### PUT hot path

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
    v
  pending_renames.insert((bucket,key), entry)   <- DashMap insert
    |
    v
  rename_tx.send(slot_id)                       <- non-blocking channel send
    |
    v
  ack to client                                 total: 2 syscalls
```

Two syscalls. The mkdir, both file creates, the close cycles, and
the meta open+write+close have all moved off the request path.

For the streaming `LocalShardWriter` path, `open_shard_writer` returns
a writer that holds the slot internally. `write_chunk` writes to the
slot's data file. `finalize` writes the meta JSON, inserts pending,
sends to the rename queue. Same overhead beyond the chunk writes
themselves.

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
  replenish slot:
    open new slot-NNNN.data.tmp
    open new slot-NNNN.meta.tmp
    push WriteSlot back into the pool
}
```

Worker syscalls per drained PUT: mkdir + 2 renames + 2 file creates
(replenish) = 5, all off the request path. **No parsing. No copying.
No re-serialization.** The two renames are not jointly atomic; the
crash window is handled by recovery (next section).

The replenish step is the only file create that happens. It's
amortized across the inter-arrival interval, not on the request path.

## `ObjectMetaFile` format change

For the worker to be a literal `mv` (no parsing, no rewriting), the
temp meta file must be byte-for-byte the destination meta.json.
That means the meta file has to know its own destination -- which
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
   `.abixio.tmp/preopen/`. Recovery scans, reads `bucket` and `key`
   from `slot-N.meta.tmp`, runs both renames. Recovered.
2. **Crash between the two renames.** `shard.dat` is at the
   destination, `meta.tmp` is still in the temp dir. Recovery scans
   the temp dir, reads the meta file to find the destination, sees
   the data file already in place, runs only the meta rename.
   Recovered.
3. **Crash after both renames, before pool replenish.** Both files
   at the destination, no temp files left. Recovery sees no temp
   pair to process. The pool init step at startup creates fresh
   slots. Recovered.

In all three cases the destination ends up with a complete
`shard.dat + meta.json` pair. **Zero data loss for any acked PUT
under process crash.**

The recovery procedure on `LocalVolume::new`, before creating the
pool:

1. List `.abixio.tmp/preopen/` and group files by slot id.
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
4. Delete any other leftovers in `.abixio.tmp/preopen/`.
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
    and replenishes the slot.
  - Otherwise log store / file tier.

A subtle race: if a GET arrives just as the rename worker is mid-rename,
the path could 404. Protect with a small retry: if `pending_renames`
says present but the data file at the temp path is gone, also check
the final file path before returning 404.

## Backpressure

- **Pool empty (no slots available).** Caller falls through to the
  existing slow path. Graceful degradation. The pool replenishes as
  the rename worker drains.
- **Rename queue depth above 256 entries per disk.** Caller falls
  through to the slow path. Prevents the rename worker from falling
  arbitrarily behind under sustained load.
- **Replenish failure.** If the worker can't open a new slot file
  (disk full, permission denied), the pool runs at degraded depth
  and the slow path takes over.

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
   64 files in `.abixio.tmp/preopen/`. Plus up to 256 in-flight
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
   the same -- which means the worker can't be a pure `mv` for
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

The pool is an alternative to the log store, not a strict
replacement. The log store keeps multiple needles per file, which
is better for tiny-object reads (better page cache density, faster
recovery). The pool keeps one file per object, which makes deletion
and space reclamation trivial (`unlink()` handles everything; no
segment compactor needed) and applies to any size, not just <=64KB.

Until benchmarks decide which one to ship as the default, both
coexist behind a runtime flag:

```
--write-tier log     # log store + file tier (current behavior)
--write-tier pool    # pool for everything, log store disabled
--write-tier file    # both disabled, baseline slow path
```

This lets us run `bench_4kb.py` and the bench matrix three ways and
make a data-driven call before deleting the loser.

## Implementation phases

| Phase | What | Delivers |
|---|---|---|
| 1 | `write_slot_pool.rs`: pool, slot, replenish | core data structures |
| 2 | `ObjectMetaFile` `bucket` and `key` fields | self-describing meta |
| 3 | Wire `write_shard` to use the pool when enabled | buffered PUT fast path |
| 4 | Wire `open_shard_writer` to use the pool | streaming PUT fast path |
| 5 | Read path integration in all five readers | reads see pending writes |
| 6 | Crash recovery scan in `LocalVolume::new` | restart safety |
| 7 | Admin endpoint `GET /_admin/preopen/status` | operator visibility |
| 8 | Tests: unit + integration + crash kill | confidence |
| 9 | Benchmark all three tiers | data-driven decision |

## Benchmark plan

Run `cd abixio-ui && cargo test --release --test bench -- --ignored
--nocapture bench_matrix` and `tests/bench_4kb.py` three times, once
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

## Open questions for implementation

- **Pool depth default.** Recommended 32 per disk. Tunable via config.
- **Windows rename semantics.** `tokio::fs::rename` uses
  `std::fs::rename`, which on Windows uses `MoveFileEx` with
  `MOVEFILE_REPLACE_EXISTING`. Verify this behaves atomically when
  the destination already exists from a half-completed crash.
- **fsync.** Default no, matching the log store and write cache.
  Document the power-loss trade-off.
- **Pre-allocation (fallocate / SetEndOfFile).** First version skips
  it. If benchmarks show extent fragmentation matters at 1GB, add
  a pre-allocation pass at slot creation.

## Files to modify (when phase 2 begins)

- `src/storage/metadata.rs` -- add `bucket` and `key` fields to
  `ObjectMetaFile` with `#[serde(default)]`. Update writers in
  `local_volume.rs` to populate them.
- New: `src/storage/write_slot_pool.rs` -- pool, slot, rename worker,
  recovery scan.
- `src/storage/mod.rs` -- declare new module.
- `src/storage/local_volume.rs` -- new `write_pool` field on
  `LocalVolume`. Modify `write_shard`, `open_shard_writer`,
  `read_shard`, `mmap_shard`, `stat_object`, `list_objects`, and
  `delete_object` to consult the pool and `pending_renames`. Spawn
  the rename worker. Run crash recovery in `new()`. Add
  `enable_write_pool(depth)` mirroring `enable_log_store()`.
- `src/storage/pathing.rs` -- helpers `preopen_dir(root)`,
  `preopen_data_path(root, slot_id)`, `preopen_meta_path(root,
  slot_id)`.
- `src/main.rs` -- new `--write-tier` CLI flag. Pass shutdown signal
  into the rename worker (reuse the `tokio::sync::watch` channel
  used by the heal worker).
- `src/admin/handlers.rs` -- new endpoint
  `GET /_admin/preopen/status` returning per-disk pool depth, queue
  depth, pending count, and replenish errors.
- `src/config.rs` -- add `write_tier` field.
- `tests/s3_integration.rs` -- pool integration tests.

## Verification

### Unit tests in `src/storage/write_slot_pool.rs`

- pool init creates N slot pairs
- consume + replenish cycle preserves depth
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
  remains in `.abixio.tmp/preopen/` and the object 404s.
- Kill the process between the two renames using a test harness,
  restart, verify the destination has both shard.dat and meta.json
  and the object is readable.

### Benchmark comparison

See the benchmark plan section above. Three runs of the bench matrix
plus `bench_4kb.py`, one per tier setting. Decision criterion: which
tier shows the best PUT obj/sec at 4KB and the best PUT MB/s at 1MB
and 10MB without regressing GET latency by more than 100us at any
size.

## See also

- [Log-structured storage](write-log.md) -- the small-object tier
  this design competes with for sizes <=64KB.
- [RAM write cache](write-cache.md) -- a different approach to
  removing disk I/O from the write path; orthogonal to the pool.
- [Architecture](architecture.md) -- where the pool fits in the
  overall design principles.
