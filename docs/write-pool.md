# Pre-opened temp file pool

An alternative write path designed to replace the
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
file per object, so reclaiming space is just `unlink()` -- the
filesystem handles it natively, no compactor needed.

The pool and the log store are alternatives at the same tier level.
The [RAM write cache](write-cache.md) sits above whichever one is
enabled. Which mechanism ships as the default is decided by benchmark;
see the comparison plan at the end of this doc.

## Why

The current file-per-object write path -- which the log store already
bypasses for objects <=64KB and the pool aims to replace entirely --
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
and both paths. A consumer pops one, fills both files, and sends a
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
worker -- crash before the send is harmless because recovery picks
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
design. These are committed defaults, not tunables:

**1. Run the two writes concurrently with `tokio::join!`.** The data
write and the meta write target two different file descriptors;
nothing forces them to be sequential. `tokio::fs` dispatches each
`write_all` to the blocking thread pool, so concurrent writes really
do run on two threads in parallel:

```rust
tokio::try_join!(
    slot.data_file.write_all(shard_bytes),
    slot.meta_file.write_all(meta_json),
)?;
```

The meta write is always tiny (~500 bytes) and finishes first; the
data write is on the critical path either way. Wall-clock cost
becomes `max(data, meta)` instead of `data + meta`. Savings: ~10us
per PUT regardless of object size. That's about 50% of the syscall
portion at 4KB, where it matters most.

**2. Compact JSON instead of pretty JSON.** `metadata.rs:170`
currently uses `serde_json::to_vec_pretty`. Switch to
`serde_json::to_vec`. The output is ~30% smaller and ~20% faster to
serialize. Nobody hand-reads meta.json files, so the whitespace has
no audience. Savings: ~150 bytes per meta file on disk and ~3-5us
of serialization time per PUT. Free; ship outside the pool work
too.

**3. Skip the tokio blocking pool for small writes.** Each
`tokio::fs::write_all` call costs ~1-2us of tokio overhead before
the actual syscall: enqueue work to the blocking pool, wake a
worker thread, signal completion. For payloads <=4KB the syscall
itself is ~10us, so the dispatch overhead is 10-20% of the cost.
For these small writes, doing `std::fs::File::write_all`
synchronously inside the async function is faster, and 10us is well
under tokio's "don't block longer than 10ms" rule of thumb:

```rust
if shard_bytes.len() <= 4096 {
    use std::io::Write;
    slot.data_file.as_std().write_all(shard_bytes)?;
    slot.meta_file.as_std().write_all(meta_json)?;
} else {
    // tokio blocking pool for large writes to avoid stalling the runtime
    tokio::try_join!(
        slot.data_file.write_all(shard_bytes),
        slot.meta_file.write_all(meta_json),
    )?;
}
```

Savings: ~2-4us per small PUT.

**Net hot path with all three.** For a 4KB PUT, the pre-ack path
becomes roughly two synchronous writes to page cache (~10us each
without dispatch overhead) plus a DashMap insert (~100ns). Total
~10-20us before ack instead of ~30-50us. The pool starts to
genuinely compete with the log store on small-object write latency.

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
   `.abixio.sys/tmp/`. Recovery scans, reads `bucket` and `key`
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

The pool is designed as a replacement for the log store. The main
motivation is GC: one file per object means `unlink()` reclaims space
natively, with no segment compactor, no live needle scan, no
copy-forward pass. The log store needs phase 8 GC -- the pool needs
zero new code.

The one known trade-off in the other direction is GET latency for
very small objects. The log store does HashMap lookup + slice from
an already-mmap'd segment in ~10us. The pool does File::open + mmap
in ~100us. The benchmark plan below quantifies the gap on real
workloads. If the gap is small in practice, the log store goes away.
If the gap is large for hot tiny objects, we can keep the log store
enabled for those and use the pool for everything else.

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

| Phase | What | Delivers | Status |
|---|---|---|---|
| 1 | `write_slot_pool.rs`: pool, slot, replenish | core data structures | **done** (63ns pop+release, see "Implementation status" below) |
| 2 | `ObjectMetaFile` `bucket` and `key` fields | self-describing meta | pending |
| 3 | Wire `write_shard` to use the pool when enabled | buffered PUT fast path | pending |
| 4 | Wire `open_shard_writer` to use the pool | streaming PUT fast path | pending |
| 5 | Read path integration in all five readers | reads see pending writes | pending |
| 6 | Crash recovery scan in `LocalVolume::new` | restart safety | pending |
| 7 | Admin endpoint `GET /_admin/pool/status` | operator visibility | pending |
| 8 | Tests: unit + integration + crash kill | confidence | pending |
| 9 | Benchmark all three tiers | data-driven decision | pending |

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

**Faster JSON serializer.** `serde_json` serializes a ~500-byte
`ObjectMetaFile` in ~5-10us. `simd-json` and `sonic-rs` are 2-3x
faster on the same workload. The destination meta.json must remain
valid JSON (so any drop-in serializer is fine), and the temp meta
file uses the same format.

Trade-off: extra dependency, additional binary size, may not work
on all targets (simd-json requires SSE4.2/AVX2 detection at runtime).

Add behind a `feature = "fast-json"` cargo flag. Measure 4KB and
64KB PUT latency with and without. Decision: enable by default if
it shows >=5us improvement on the small-PUT path, otherwise leave
as opt-in.

## Implementation status

### Phase 1: pool primitive in isolation -- DONE

Landed in commit alongside this doc update. The new
`src/storage/write_slot_pool.rs` contains `WriteSlotPool::new`,
`try_pop`, `release`, and `available` -- the bare slot pool, no
rename worker, no `pending_renames`, no integration. Backed by
`crossbeam_queue::ArrayQueue` (lock-free MPMC).

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
the `Instant::now()` resolution on this hardware -- the operation
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

### Phase 2-9: pending

See `Implementation phases` table above. Phase 2 (slot writes with
real I/O, no orchestration) is the next step.

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
- `src/storage/pathing.rs` -- helpers `pool_dir(root)`,
  `pool_data_path(root, slot_id)`, `pool_meta_path(root, slot_id)`.
  All return paths under `.abixio.sys/tmp/`.
- `src/main.rs` -- new `--write-tier` CLI flag. Pass shutdown signal
  into the rename worker (reuse the `tokio::sync::watch` channel
  used by the heal worker).
- `src/admin/handlers.rs` -- new endpoint
  `GET /_admin/pool/status` returning per-disk pool depth, queue
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
  remains in `.abixio.sys/tmp/` and the object 404s.
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
