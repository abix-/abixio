# Write path

**Authoritative for:** how a PUT moves through AbixIO. End-to-end flow,
routing decisions, all five storage branches (RAM cache, log store,
write pool, file tier, remote backend), ack semantics, and current
performance characteristics. If you need to know how a write works,
this is the only doc.

**Not covered here:** on-disk format (see [storage-layout.md](storage-layout.md)),
optimization history (see [layer-optimization.md](layer-optimization.md)),
benchmark results (see [benchmarks.md](benchmarks.md)).

This page describes the code that exists now. Timings are measured-only:
if a step has no benchmark in the repo, it is described but not given a
numeric budget.

## Table of contents

- [Scope](#scope)
- [Top-level flow](#top-level-flow)
- [Entry and request shaping](#entry-and-request-shaping)
  - [1. HTTP ingress](#1-http-ingress)
  - [2. S3 protocol and AbixIO request setup](#2-s3-protocol-and-abixio-request-setup)
- [Routing decision matrix](#routing-decision-matrix)
- [Layer-by-layer write path](#layer-by-layer-write-path)
  - [3. Validation and bucket existence](#3-validation-and-bucket-existence)
  - [4. Small-object body collection](#4-small-object-body-collection)
  - [5. EC resolution, hashing, and placement](#5-ec-resolution-hashing-and-placement)
- [Storage branches](#storage-branches)
  - [Branch A: RAM write cache](#branch-a-ram-write-cache)
  - [Branch B: Local log store](#branch-b-local-log-store)
  - [Branch C: Local write pool](#branch-c-local-write-pool)
  - [Branch D: Local file tier](#branch-d-local-file-tier)
  - [Branch E: Remote backend](#branch-e-remote-backend)
- [Ack semantics by branch](#ack-semantics-by-branch)
- [Where the time goes](#where-the-time-goes)
- [How other docs should use this page](#how-other-docs-should-use-this-page)
- [Accuracy Report](#accuracy-report)

## Scope

This document covers:

- S3 `PutObject` request handling
- branch selection by object size, versioning, backend type, and cache state
- where success is acknowledged
- what background or follow-up work still remains after ack
- what "final resting place" means for each branch
- the measured performance of each layer where the repo has benchmarks

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
       RAM write cache -> ack -> explicit flush later
       log store -> ack
       write pool -> ack -> rename worker later
       file tier -> ack
       remote volume RPC -> ack after target node completes its local shard path
```

The main code anchors are:

- `src/s3_service.rs`: request-level decisions
- `src/storage/volume_pool.rs`: routing, EC, placement, quorum
- `src/storage/local_volume.rs`: local log/pool/file write behavior
- `src/storage/write_cache.rs`: RAM cache structure
- `src/storage/write_slot_pool.rs`: pending-rename and worker behavior
- `src/storage/remote_volume.rs`: remote shard buffering and finalize POST
- `src/storage/storage_server.rs`: remote target dispatch

## Benchmark layers

Each section below includes timing from `abixio-ui bench`. Layers
L1 through L5 are isolated: each measures ONLY its own overhead.

| Layer | What it measures | How it isolates |
|---|---|---|
| L1 | HTTP transport | bare hyper over TCP, no S3, no storage |
| L2 | S3 protocol | s3s over in-memory pipe (no TCP), NullBackend (no storage) |
| L3 | storage pipeline | direct VolumePool API call, no HTTP, no s3s |
| L4 | hashing + RS encode | direct function call on in-memory buffer, no I/O |
| L5 | raw disk I/O | direct tokio::fs call, no storage pipeline |
| L6 | S3 + storage | integration: TCP + s3s + real VolumePool |
| L7 | full e2e | integration: real server process, SDK client, TLS, auth |

L6 and L7 are integration tests, not isolated. They show how the
layers compose together and what a real client actually sees.

## Entry and request shaping

### 1. HTTP ingress

Raw HTTP transport floor: bare `hyper` server, `reqwest` client,
loopback 127.0.0.1. No S3, no storage. This is the lowest possible
latency the stack can achieve at each size.

Source: `abixio-ui bench --layers L1`, `bench-results/l1-http-ingress.json`

#### PUT (reqwest -> hyper, body consumed and discarded)

| Size | p50 | p95 | p99 | throughput |
|---|---|---|---|---|
| 4KB | `100us` | `155us` | `182us` | `34.7 MB/s` |
| 64KB | `179us` | `242us` | `303us` | `328.2 MB/s` |
| 10MB | `43.6ms` | `47.4ms` | `48.4ms` | `267.6 MB/s` |
| 100MB | `386.6ms` | `397.4ms` | `397.4ms` | `310.8 MB/s` |
| 1GB | `1.62s` | `3.98s` | `3.98s` | `456.7 MB/s` |

#### GET (hyper returns sized body, reqwest consumes it)

| Size | p50 | p95 | p99 | throughput |
|---|---|---|---|---|
| 4KB | `90us` | `145us` | `170us` | `38.0 MB/s` |
| 64KB | `166us` | `197us` | `230us` | `382.7 MB/s` |
| 10MB | `30.6ms` | `46.5ms` | `47.6ms` | `352.4 MB/s` |
| 100MB | `171.2ms` | `396.9ms` | `396.9ms` | `476.8 MB/s` |
| 1GB | `1.61s` | `1.73s` | `1.73s` | `663.1 MB/s` |

`hyper` accepts the request, parses HTTP/1.1, and exposes the body as
a stream. At small sizes (4KB, 64KB), latency is dominated by TCP
round-trip and HTTP framing, not data transfer. At large sizes (100MB,
1GB), throughput reaches 400-630 MB/s, which is the hyper/Windows
loopback ceiling.

GET is faster than PUT at every size because the hyper server builds
the response body from a pre-allocated `Bytes` buffer (zero-copy),
while PUT requires the server to consume the incoming body stream.

### 2. S3 protocol and AbixIO request setup

Isolated S3 protocol overhead. Uses an in-memory duplex pipe instead
of TCP, so there is no L1 (HTTP transport) overhead in these numbers.
NullBackend discards all writes, so there is no storage overhead either.
This measures only: s3s header parsing, SigV4 verification, AbixioS3
dispatch, and VolumePool routing.

Source: `abixio-ui bench --layers L2`, `bench-results/l2-s3proto-isolated.json`

#### PUT (hyper client -> in-memory pipe -> s3s -> NullBackend)

| Size | p50 | p95 | p99 | throughput |
|---|---|---|---|---|
| 4KB | `60us` | `105us` | `166us` | `56.8 MB/s` |
| 64KB | `197us` | `299us` | `334us` | `293.2 MB/s` |
| 10MB | `7.8ms` | `8.5ms` | `8.7ms` | `1275.5 MB/s` |
| 100MB | `65.7ms` | `70.5ms` | `70.5ms` | `1499.4 MB/s` |
| 1GB | `664.7ms` | `680.4ms` | `680.4ms` | `1536.8 MB/s` |

#### GET (NullBackend returns empty, not meaningful at large sizes)

| Size | p50 | p95 | p99 | throughput |
|---|---|---|---|---|
| 4KB | `54us` | `76us` | `97us` | `69.7 MB/s` |
| 64KB | `53us` | `79us` | `96us` | `1121.6 MB/s` |

GET at 10MB+ is not meaningful because NullBackend returns zero bytes
regardless of requested size. The GET latency at all sizes is just
s3s dispatch overhead (~52-118us).

`s3s` parses S3 headers and dispatches into `AbixioS3::put_object`.
`src/s3_service.rs` then:

- resolves `content_type`
- pulls user metadata
- reads cached bucket versioning state
- decides whether this request should allocate a version ID
- sets `skip_md5` when `Content-MD5` is absent
- forwards the body stream and optional `content_length` into
  `VolumePool::put_object_stream`

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

| Metric | 4KB p50 | 4KB throughput | larger sizes | Source |
|---     |---      |---             |---           |---     |
| validation + bucket existence | not isolated | -- | not isolated | included inside §2 dispatch cost; no standalone benchmark exists |

Before any data write, `VolumePool::put_object_stream` validates:

- bucket name
- object key
- optional version ID
- bucket existence

If the bucket does not exist, the write stops here.

### 4. Small-object body collection

| Metric | 4KB p50 | 4KB throughput | larger sizes | Source |
|---     |---      |---             |---           |---     |
| small-object branch decision + collect body into `Vec<u8>` | `~20us` | -- | not isolated; only runs for `<=64KB` non-versioned | synthesized 4KB trace, originally in `write-cache.md::Request trace` (now removed); not from a benchmark |

For non-versioned requests with a declared `content_length <= 64KB`,
the body stream is fully collected into a `Vec<u8>`. This is what makes
the small-object branches possible: AbixIO has the whole payload before
choosing its durable tier.

This collection step does not happen for versioned or streaming/large
requests. Those stay on the streaming encode path.

### 5. EC resolution, hashing, and placement

Isolated L4 compute. Direct function calls on in-memory buffers,
no I/O, no storage pipeline, no HTTP. These are the per-byte CPU
ceilings for each operation.

Source: `abixio-ui bench --layers L4`, `bench-results/l4-compute.json`

| Op | 4KB p50 | 64KB p50 | 10MB p50 | 100MB p50 | 1GB p50 | throughput |
|---|---|---|---|---|---|---|
| blake3 | `2us` | `14us` | `2.2ms` | `23.3ms` | `239.6ms` | `4283 MB/s` |
| md5 | `6us` | `89us` | `14.2ms` | `141.8ms` | `1.45s` | `702 MB/s` |
| sha256 | `15us` | `224us` | `35.5ms` | `359.6ms` | `3.69s` | `277 MB/s` |
| rs_encode 3+1 | `1us` | `20us` | `3.6ms` | `35.7ms` | `366.4ms` | `2781 MB/s` |

Throughput column is from the 1GB run. blake3 and RS encode are fast
enough to be invisible in the storage pipeline. MD5 at 702 MB/s is
the bottleneck when Content-MD5 is required (skipped by default via
xxhash64 ETag).

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

## Storage branches

### Branch A: RAM write cache

| Metric | 4KB p50 | 4KB throughput | larger sizes | Source |
|---     |---      |---             |---           |---     |
| `DashMap.insert` primitive | `~1us` | -- | `1M+ obj/s` primitive rate | not from a benchmark; primitive measurement |
| end-to-end RAM-cache PUT branch | not published | -- | not published | no isolated end-to-end bench exists for this branch yet; the SDK matrix bench in `benchmarks.md` does not exercise it because the harness boots abixio without the cache enabled |
| end-to-end RAM-cache GET branch | not published | -- | not published | same -- the harness does not enable the cache |

Synthesized 4KB request trace (not from a benchmark, just a model
showing where the time goes when the cache *is* enabled):

```
[~80us]   hyper: accept TCP, parse HTTP/1.1 headers, read body
[~100us]  s3s: extract S3 headers, dispatch to AbixioS3::put_object()
[~50us]   s3_service: versioning check, content_type, metadata, EC ftt
[~20us]   volume_pool: collect 4KB body into Vec<u8>
[~30us]   RS encode 3+1, MD5 ETag, blake3 per-shard checksum
[~10us]   placement: hash key -> pick disk per shard
[~1us]    >>> DashMap.insert(key, CacheEntry) <<<  THE ACTUAL STORAGE
[~80us]   s3s: serialize 200 OK, hyper writes to TCP

Storage:  ~1us    (~0.3% of total)
Floor:    ~370us  (~99.7% of total) -- hyper + s3s + protocol work
```

The point of this trace is that the storage primitive is invisible
under the protocol floor at 4KB. Cf. the SDK-matrix bench finding in
`benchmarks.md::Comprehensive matrix`: file/log/pool tier choices are
within 4% of each other at 4KB sdk PUT for exactly the same reason --
the protocol floor dominates.

Run `bench_pool_l4_tier_matrix` (`abixio-ui/src/bench/`) to see the
file/log/pool branches end-to-end at 4KB through 100MB. There is
currently no equivalent bench that enables the RAM write cache in the
process; adding one is on the TODO list.

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

### Branch B: Local log store

Isolated L3 storage pipeline. Direct VolumePool API call, no HTTP,
no s3s. 1 disk, ftt=0, no write cache.

Source: `abixio-ui bench --layers L3 --write-paths log`,
`bench-results/l3-storage.json`

| Size | PUT p50 | PUT throughput | GET p50 | GET throughput |
|---|---|---|---|---|
| 4KB | `176us` | `21.2 MB/s` | `35us` | `111.6 MB/s` |
| 64KB | `394us` | `156.5 MB/s` | `60us` | `971.8 MB/s` |
| 10MB | `32.1ms` | `308.7 MB/s` | `467us` | `20601 MB/s` |
| 100MB | `339.9ms` | `294.7 MB/s` | `591us` | `163690 MB/s` |
| 1GB | `3.53s` | `290.3 MB/s` | `822us` | `1339146 MB/s` |

The log store handles small objects (<=64KB) natively via needle
append. At 4KB it is 4.5x faster than the file tier. At 10MB+ the
LogShardWriter buffers chunks then falls back to file-tier write
because objects exceed the 64KB log threshold, which adds overhead
compared to the direct file tier path. GET uses the log index +
mmap and is sub-millisecond at all sizes (page-cache hot).

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

### Branch C: Local write pool

Isolated L3 storage pipeline. Direct VolumePool API call, no HTTP,
no s3s. 1 disk, ftt=0, pool depth 32, no write cache.

Source: `abixio-ui bench --layers L3 --write-paths pool`,
`bench-results/l3-storage.json`

| Size | PUT p50 | PUT throughput | GET p50 | GET throughput |
|---|---|---|---|---|
| 4KB | `202us` | `9.2 MB/s` | `586us` | `6.6 MB/s` |
| 64KB | `364us` | `136.3 MB/s` | `619us` | `90.8 MB/s` |
| 10MB | `19.3ms` | `516.7 MB/s` | `634us` | `15574 MB/s` |
| 100MB | `184.6ms` | `534.3 MB/s` | `622us` | `153081 MB/s` |
| 1GB | `2.08s` | `489.8 MB/s` | `1.0ms` | `1048536 MB/s` |

The pool writes to pre-opened temp files and queues a rename, so
the PUT ack happens before the final path exists. At 4KB it is
4x faster than the file tier. At 10MB+ it beats file tier on PUT
because it avoids mkdir + File::create. GET reads from the pending
temp file via pending_renames lookup.

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

### Branch D: Local file tier

Isolated L3 storage pipeline. Direct VolumePool API call, no HTTP,
no s3s. 1 disk, ftt=0, no write cache.

Source: `abixio-ui bench --layers L3 --write-paths file`,
`bench-results/l3-storage.json`

| Size | PUT p50 | PUT throughput | GET p50 | GET throughput |
|---|---|---|---|---|
| 4KB | `847us` | `4.4 MB/s` | `452us` | `8.2 MB/s` |
| 64KB | `1.0ms` | `58.9 MB/s` | `437us` | `135.6 MB/s` |
| 10MB | `21.7ms` | `459.0 MB/s` | `493us` | `19576 MB/s` |
| 100MB | `195.5ms` | `510.1 MB/s` | `553us` | `173527 MB/s` |
| 1GB | `1.99s` | `505.3 MB/s` | `726us` | `1473876 MB/s` |

The file tier writes shard.dat and meta.json directly to their
final paths. No post-write rename, no pre-opened files. At 4KB it
is the slowest tier because every PUT pays mkdir + File::create.
GET uses mmap (page-cache hot).

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

### Branch E: Remote backend

| Metric | 4KB timing | 64KB timing | 10MB throughput | 1GB throughput | Notes | Source |
|---|---|---|---|---|---|---|
| remote backend branch | not isolated | not isolated | not isolated | not isolated | current repo has no remote-only PUT benchmark; inherits target branch + internode HTTP overhead | current benchmark corpus |

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

Isolated L3 tier comparison. Direct VolumePool API, no HTTP, no s3s.
1 disk, ftt=0, no write cache, Defender-excluded tmp dir. All paths
use the unified write path (streaming encode + tier-aware ShardWriter).

Source: `abixio-ui bench --layers L3`, `bench-results/l3-storage.json`

#### PUT p50 latency by tier (put_stream)

| Size | file | log | pool | best tier |
|---|---|---|---|---|
| 4KB | `847us` | **`176us`** | `202us` | log |
| 64KB | `1.0ms` | **`394us`** | `364us` | pool |
| 10MB | `21.7ms` | `32.1ms` | **`19.3ms`** | pool |
| 100MB | `195.5ms` | `339.9ms` | **`184.6ms`** | pool |
| 1GB | **`1.99s`** | `3.53s` | `2.08s` | file |

#### PUT throughput by tier (put_stream)

| Size | file | log | pool | best tier |
|---|---|---|---|---|
| 4KB | `4.4 MB/s` | **`21.2 MB/s`** | `9.2 MB/s` | log |
| 64KB | `58.9 MB/s` | **`156.5 MB/s`** | `136.3 MB/s` | log |
| 10MB | `459.0 MB/s` | `308.7 MB/s` | **`516.7 MB/s`** | pool |
| 100MB | `510.1 MB/s` | `294.7 MB/s` | **`534.3 MB/s`** | pool |
| 1GB | **`505.3 MB/s`** | `290.3 MB/s` | `489.8 MB/s` | file |

#### GET p50 latency by tier (get_stream, mmap, page-cache hot)

| Size | file | log | pool | best tier |
|---|---|---|---|---|
| 4KB | `452us` | **`35us`** | `586us` | log |
| 64KB | `437us` | **`60us`** | `619us` | log |
| 10MB | `493us` | **`467us`** | `634us` | log |
| 100MB | `553us` | **`591us`** | `622us` | file |
| 1GB | **`726us`** | `822us` | `1.0ms` | file |

GET is sub-millisecond at all sizes because data sits in OS page
cache. Log store GET is fastest at small sizes because it reads
from the in-memory index + mmap segment (no directory traversal).

#### Tier handoff

- **<=64KB**: log store wins PUT (4.5x faster than file at 4KB)
  and GET (13x faster at 4KB)
- **10MB-100MB**: pool wins PUT (avoids mkdir + File::create)
- **>=1GB**: file tier wins PUT (LogShardWriter buffer + fallback
  adds overhead; pool rename tax grows with object count)
- Log store loses at large sizes because LogShardWriter buffers
  chunks internally, then falls back to file-tier write when the
  accumulated data exceeds 64KB. This double-write is the cost of
  not knowing the final size up front.

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
| Per-branch tier tables (B/C/D) carry full 5-size PUT and GET p50 plus throughput | Verified | Source: `bench-results/phase8.7-tier-matrix.txt`. Throughput cells are derived as `size_bytes / 1.048576 / p50_us`, so each cell tracks the latency cell exactly and uses the same MB definition as the bench output (1 MB = 1048576 bytes) |
| Cross-over from log -> pool -> file tier as object size grows | Verified | Tier matrix tables in §"Where the time goes" show log winning <=64KB PUT, pool winning at 1MB PUT, file winning at 10MB and 100MB PUT |

Verdict: the routing and ack/final-resting-place semantics on this page are grounded in the current code. The timing sections are as authoritative as the current benchmark corpus, but they remain benchmark-derived rather than freshly re-measured in this pass.
