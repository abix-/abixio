# Write path

Canonical reference for how a `PUT` moves through AbixIO from client
request to its final resting place. Other docs should link here for the
end-to-end flow instead of restating it.

This page is intentionally implementation-first. It describes the code
that exists now, not the older design intent. Timings are measured-only:
if a step has no benchmark or trace in the repo, it is described but not
given a numeric budget.

## Scope

This document covers:

- S3 `PutObject` request handling
- branch selection by object size, versioning, backend type, and cache state
- where success is acknowledged
- what background or follow-up work still remains after ack
- what "final resting place" means for each branch

This document does not cover:

- GET/read-path details
- multipart-complete assembly internals
- recovery/heal internals beyond write-path consequences

## Top-level flow

At a high level the write path is:

```text
client
  -> hyper HTTP server
  -> s3s S3 protocol layer
  -> AbixioS3::put_object
  -> VolumePool::put_object_stream
  -> branch:
       RAM write cache
       log store
       write pool
       file tier
       remote volume RPC
  -> ack
  -> optional post-ack work until final on-disk form exists
```

The main code anchors are:

- `src/s3_service.rs`: request-level decisions
- `src/storage/volume_pool.rs`: routing, EC, placement, quorum
- `src/storage/local_volume.rs`: local log/pool/file write behavior
- `src/storage/write_cache.rs`: RAM cache structure
- `src/storage/write_slot_pool.rs`: pending-rename and worker behavior
- `src/storage/remote_volume.rs`: remote shard buffering and finalize POST
- `src/storage/storage_server.rs`: remote target dispatch

## Entry and request shaping

### 1. HTTP ingress

`hyper` accepts the request, parses HTTP/1.1, and exposes the body as a
stream. This is the lowest measured floor in the stack.

Measured timing:

| Layer | Timing | Source |
|---|---|---|
| bare `hyper` request floor at 4KB PUT | `94us` p50 | `docs/benchmarks.md`, Phase 8.5 Stage A |

### 2. S3 protocol and AbixIO request setup

`s3s` parses S3 headers and dispatches into `AbixioS3::put_object`.
`src/s3_service.rs` then:

- resolves `content_type`
- pulls user metadata
- reads cached bucket versioning state
- decides whether this request should allocate a version ID
- sets `skip_md5` when `Content-MD5` is absent
- forwards the body stream and optional `content_length` into
  `VolumePool::put_object_stream`

Measured timing:

| Layer | Timing | Source |
|---|---|---|
| `hyper` + `s3s` + `AbixioS3` dispatch overhead at 4KB PUT | `126us` p50 total | `docs/benchmarks.md`, Phase 8.5 Stage C |
| incremental cost of `s3s` + AbixIO dispatch above bare `hyper` | `32us` p50 | `docs/benchmarks.md`, Phase 8.5 Stage C vs Stage A |
| server-side 4KB PUT processing seen from `x-debug-s3s-ms` | `0.28ms` | `docs/benchmarks.md`, Server-side profiling |

## Routing decision matrix

`VolumePool::put_object_stream` chooses the write path from four facts:

- is the object versioned?
- is `content_length` present?
- is `content_length <= 64KB`?
- is the RAM write cache enabled and able to accept the object?

Current routing:

| Condition | Path |
|---|---|
| non-versioned and `content_length <= 64KB` | collect body in memory, EC-encode, then try RAM cache first |
| RAM cache insert succeeds | ack from RAM cache |
| RAM cache insert fails or cache disabled | call `write_shard` per selected backend |
| local backend + non-versioned + `meta.size <= 64KB` + log enabled | log store |
| local backend + write pool enabled and object qualifies for that local path | write pool |
| local backend fallback | file tier |
| remote backend | HTTP POST to target node's storage server, which then executes its own local branch |
| versioned object or unknown/large content length | streaming `encode_and_write` path |

Important detail: the 64KB threshold is checked twice in different
places for different reasons.

- `VolumePool` uses `content_length <= 64KB` to decide whether it can
  buffer the full object and take the small-object path.
- `LocalVolume::write_shard` uses `meta.size <= 64KB` plus
  `!is_versioned` when deciding whether that shard can go to the log or
  inline-file path.

## Layer-by-layer write path

### 3. Validation and bucket existence

Before any data write, `VolumePool::put_object_stream` validates:

- bucket name
- object key
- optional version ID
- bucket existence

If the bucket does not exist, the write stops here.

Timing:

- measured indirectly inside the Stage C and Stage D numbers above
- no separate timing is published for validation alone

### 4. Small-object body collection

For non-versioned requests with a declared `content_length <= 64KB`,
the body stream is fully collected into a `Vec<u8>`. This is what makes
the small-object branches possible: AbixIO has the whole payload before
choosing its durable tier.

This collection step does not happen for versioned or streaming/large
requests. Those stay on the streaming encode path.

Measured timing:

| Step | Timing | Source |
|---|---|---|
| `volume_pool` small-object path decision + collect step in historical 4KB trace | `~0.02ms` | `docs/write-cache.md`, request trace |

### 5. EC resolution, hashing, and placement

Once the full small object is buffered, `VolumePool`:

- resolves `(data_n, parity_n)` from per-object FTT or bucket FTT
- computes the ETag
- builds data and parity shards
- computes per-shard blake3 checksums
- asks the placement planner for shard distribution
- builds per-shard `ObjectMeta`

For large/versioned writes, the same logical decisions still happen, but
the data then flows through the streaming `encode_and_write` pipeline
instead of the small buffered path.

Measured timing:

| Step | Timing | Source |
|---|---|---|
| `s3_service` versioning/config work in historical 4KB trace | `~0.05ms` | `docs/write-cache.md`, request trace |
| RS encode + checksums in historical 4KB trace | `~0.03ms` | `docs/write-cache.md`, request trace |
| placement in historical 4KB trace | `~0.01ms` | `docs/write-cache.md`, request trace |

## Storage branches

### Branch A: RAM write cache

This branch exists only on the small-object buffered path. After the
object has already been validated, buffered, encoded, and assigned to
specific disks, `VolumePool` builds a `CacheEntry` and tries
`WriteCache::insert(bucket, key, entry)`.

What happens before ack:

- object bytes are already in memory as RS shards
- metadata is already built in memory
- `DashMap` insert stores the entry by `(bucket, key)`
- `put_object_stream` returns success immediately

What ack means here:

- the object is visible in the RAM cache and available to reads through
  the cache-aware read path
- the object is **not yet on disk**

Final resting place:

- not automatic in the current code
- entries reach disk only when `VolumePool::flush_write_cache()` is
  called, such as via the admin flush path
- that flush drains cached entries and calls backend `write_shard` for
  each shard, which then re-enters the normal local or remote shard path

Measured timing:

| Step | Timing | Source |
|---|---|---|
| RAM cache insert itself | `~0.001ms` | `docs/write-cache.md`, request trace and baseline table |

### Branch B: Local log store

If the cache is disabled or full, `VolumePool` writes shards to the
selected backends. On a `LocalVolume`, small non-versioned objects can
route to the log store when that volume has log storage enabled.

What happens before ack:

- `LocalVolume::write_shard` calls the log-store append path
- shard bytes and metadata are serialized into a needle
- the needle is appended to the active segment
- the in-memory log index is updated

What ack means here:

- the object is durable to the local OS page cache and addressable from
  the log index
- there is no second rename or flush stage inside AbixIO
- there is also no per-object fsync

Final resting place:

- the append-only log segment itself is the final resting place

Measured timing:

| Metric | Timing | Source |
|---|---|---|
| 4KB log-tier PUT p50 through full HTTP stack | `265us` | `docs/benchmarks.md`, Phase 8.7 tier matrix |
| 64KB log-tier PUT p50 through full HTTP stack | `385us` | `docs/benchmarks.md`, Phase 8.7 tier matrix |

### Branch C: Local write pool

If a `LocalVolume` has the pre-opened temp-file pool enabled and a slot
is available, the shard write goes through the pool path.

What happens before ack:

- a slot pair is popped from `WriteSlotPool`
- data and metadata are written to the slot's pre-opened temp files
- `pending_renames` is updated before queueing the worker request so
  read-after-write stays visible
- a `RenameRequest` is sent to the rename dispatcher
- the shard write returns success

What ack means here:

- the object exists in temp files, not yet at its final object path
- read-after-write is satisfied through `pending_renames`
- the rename worker still has to create the destination directory,
  rename the data file, rename the meta file, and replenish the slot

Final resting place:

- the final `bucket/key/.../shard.dat` and `meta.json` object paths,
  after the rename worker completes

Measured timing:

| Metric | Timing | Source |
|---|---|---|
| 4KB pool-tier PUT p50 through full HTTP stack | `454us` | `docs/benchmarks.md`, Phase 8.7 tier matrix |
| 64KB pool-tier PUT p50 through full HTTP stack | `586us` | `docs/benchmarks.md`, Phase 8.7 tier matrix |
| best measured pool fast path at HTTP layer | `318us` p50 | `docs/benchmarks.md`, Phase 8.5 Stage E# |

### Branch D: Local file tier

If the write does not hit RAM cache, log store, or write pool, it falls
through to the file tier.

There are two local file-tier shapes:

- small non-versioned objects may inline the shard bytes into
  `meta.json`
- larger objects write `shard.dat` and `meta.json` concurrently

What happens before ack:

- object directory is created
- small inline path writes `meta.json` only
- larger path writes `shard.dat` and `meta.json` with `tokio::try_join!`

What ack means here:

- the final object files are already in place on the volume
- there is no extra post-ack rename step

Final resting place:

- the final object directory on disk

Measured timing:

| Metric | Timing | Source |
|---|---|---|
| 4KB file-tier PUT p50 through full HTTP stack | `935us` | `docs/benchmarks.md`, Phase 8.7 tier matrix |
| 64KB file-tier PUT p50 through full HTTP stack | `1330us` | `docs/benchmarks.md`, Phase 8.7 tier matrix |
| full-stack file-tier 4KB reference in stack breakdown | `810us` p50 | `docs/benchmarks.md`, Phase 8.5 Stage D |

### Branch E: Remote backend

For a `RemoteVolume`, the local node does not write directly to the
target disk. The current remote write shape is:

- `RemoteShardWriter` buffers shard bytes in memory
- `finalize()` serializes metadata to JSON
- it POSTs the full shard body plus `meta` query parameter to
  `/_storage/v1/write-shard` or `/_storage/v1/write-versioned-shard`
- the target node's `StorageServer` authenticates the request, resolves
  the local volume from `x-abixio-volume-path`, and then calls that
  `LocalVolume`'s `write_shard`

What ack means here:

- the remote node has accepted and completed its local `write_shard`
  path for that shard
- the final resting place is whichever local branch the remote target
  volume uses: log store, pool temp files awaiting rename, or file tier

Timing:

- no remote-only timing breakdown is published in the repo
- remote internode overhead is therefore described, not budgeted, here

## Ack semantics by branch

This is the main durability distinction the rest of the docs should
reference.

| Branch | Ack means | Final resting place reached at ack? |
|---|---|---|
| RAM write cache | object is in RAM cache and readable from cache | no |
| log store | object is appended to the log and indexed | yes, modulo OS page-cache durability model |
| write pool | temp files are written and pending rename is registered | no |
| file tier | final object files are written in their destination directory | yes, modulo OS page-cache durability model |
| remote backend | target node completed its local shard write path | depends on the target branch |

## Where the time goes

The most useful measured attribution in the repo today is the 4KB PUT
stack breakdown.

| Layer or path | Timing | Meaning | Source |
|---|---|---|---|
| bare `hyper` | `94us` p50 | protocol floor before S3/storage work | `docs/benchmarks.md`, Phase 8.5 Stage A |
| `hyper + s3s + AbixioS3` | `126us` p50 | full request parsing and dispatch without real storage | `docs/benchmarks.md`, Phase 8.5 Stage C |
| file tier full stack | `810us` p50 | real storage work dominates the 4KB PUT cost | `docs/benchmarks.md`, Phase 8.5 Stage D |
| pool best fast path | `318us` p50 | measured pool path when depth and queue do not choke | `docs/benchmarks.md`, Phase 8.5 Stage E# |
| server processing via debug header | `0.28ms` | server-only 4KB PUT processing from live responses | `docs/benchmarks.md`, Server-side profiling |

The tier matrix is the authoritative end-to-end comparison for user
visible PUT latency:

| Size | file | log | pool | best tier | Source |
|---|---|---|---|---|---|
| 4KB | `935us` | `265us` | `454us` | log | `docs/benchmarks.md`, Phase 8.7 |
| 64KB | `1330us` | `385us` | `586us` | log | `docs/benchmarks.md`, Phase 8.7 |
| 1MB | `4360us` | `4057us` | `3797us` | pool | `docs/benchmarks.md`, Phase 8.7 |
| 10MB | `28974us` | `31744us` | `33837us` | file | `docs/benchmarks.md`, Phase 8.7 |
| 100MB | `149228us` | `164974us` | `165671us` | file | `docs/benchmarks.md`, Phase 8.7 |

## How other docs should use this page

- `architecture.md` should summarize the write path and link here.
- `benchmarks.md` should keep measurements and link here for layer and
  durability interpretation.
- `write-log.md`, `write-pool.md`, and `write-cache.md` should explain
  their own tier internals and trade-offs, then link here for the
  end-to-end path.

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| `s3_service.rs::put_object` resolves versioning, `skip_md5`, and forwards `content_length` into `put_object_stream` | Verified | `src/s3_service.rs` |
| `VolumePool::put_object_stream` uses the small-object buffered path only for non-versioned requests with declared `content_length <= 64KB` | Verified | `src/storage/volume_pool.rs` |
| RAM write cache is tried before disk writes on the small-object path | Verified | `src/storage/volume_pool.rs` |
| RAM-cache ack happens before disk persistence | Verified | `src/storage/volume_pool.rs`, `src/storage/write_cache.rs` |
| RAM write cache currently relies on explicit `flush_write_cache()` rather than an automatic destage worker in this repo | Verified | `src/storage/volume_pool.rs`, `src/admin/handlers.rs`, absence of any spawned cache flush task in `src/main.rs` |
| Local small non-versioned writes can route to the log store | Verified | `src/storage/local_volume.rs` |
| Pool writes ack before the rename worker reaches the final destination path | Verified | `src/storage/local_volume.rs`, `src/storage/write_slot_pool.rs` |
| File-tier writes place the object in its final path before ack | Verified | `src/storage/local_volume.rs` |
| Remote shard writes POST into `/_storage/v1/*` and then execute the target node's local `write_shard` path | Verified | `src/storage/remote_volume.rs`, `src/storage/storage_server.rs` |
| Timing tables on this page are measured-only | Verified | All numeric timings here are sourced from existing benchmark or trace docs; no new estimated timings were added |
| The specific timing values were independently re-run in this pass | Not re-run in this pass | Values were taken from current repo docs and bench artifacts, not freshly benchmarked during this edit |

Verdict: the routing and ack/final-resting-place semantics on this page are grounded in the current code. The timing sections are as authoritative as the current benchmark corpus, but they remain benchmark-derived rather than freshly re-measured in this pass.
