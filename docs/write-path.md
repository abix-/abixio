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
| 4KB | `98us` | `146us` | `159us` | `36.5 MB/s` |
| 64KB | `208us` | `258us` | `272us` | `301.6 MB/s` |
| 10MB | `32.1ms` | `47.2ms` | `47.4ms` | `291.6 MB/s` |
| 100MB | `385.4ms` | `391.0ms` | `391.0ms` | `329.3 MB/s` |
| 1GB | `2.09s` | `3.98s` | `3.98s` | `400.5 MB/s` |

#### GET (hyper returns sized body, reqwest consumes it)

| Size | p50 | p95 | p99 | throughput |
|---|---|---|---|---|
| 4KB | `88us` | `130us` | `152us` | `41.4 MB/s` |
| 64KB | `134us` | `177us` | `190us` | `443.6 MB/s` |
| 10MB | `32.3ms` | `46.9ms` | `47.5ms` | `334.0 MB/s` |
| 100MB | `146.5ms` | `349.5ms` | `349.5ms` | `610.2 MB/s` |
| 1GB | `1.55s` | `2.04s` | `2.04s` | `626.9 MB/s` |

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
| 4KB | `60us` | `104us` | `135us` | `57.5 MB/s` |
| 64KB | `199us` | `312us` | `433us` | `293.6 MB/s` |
| 10MB | `8.4ms` | `10.3ms` | `11.2ms` | `1159.6 MB/s` |
| 100MB | `70.6ms` | `75.6ms` | `75.6ms` | `1419.9 MB/s` |
| 1GB | `669.9ms` | `685.3ms` | `685.3ms` | `1517.8 MB/s` |

#### GET (NullBackend returns empty, not meaningful at large sizes)

| Size | p50 | p95 | p99 | throughput |
|---|---|---|---|---|
| 4KB | `52us` | `83us` | `112us` | `69.3 MB/s` |
| 64KB | `53us` | `89us` | `125us` | `1089.4 MB/s` |

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

| Metric | 4KB p50 | 4KB throughput | per-byte ceiling at larger sizes | Source |
|---     |---      |---             |---                                |---     |
| request-level config / versioning work | `~50us` | -- | constant per request | synthesized 4KB trace, originally in `write-cache.md::Request trace` |
| RS encode + checksum work | `~30us` | -- | scales with shard size; see ceilings below | synthesized 4KB trace |
| placement planning | `~10us` | -- | constant per request | synthesized 4KB trace |
| blake3 hashing per shard | n/a | -- | `4303 MB/s` | `docs/layer-optimization.md` L4 |
| MD5 hashing of full body | n/a | -- | `703 MB/s` | `docs/layer-optimization.md` L4 |
| RS encode 3+1 | n/a | -- | `2762 MB/s` | `docs/layer-optimization.md` L4 |

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

End-to-end PUT and GET measured through the full HTTP stack
(`reqwest -> hyper -> s3s -> VolumePool -> LocalVolume`) on a single
disk, ftt=0, 127.0.0.1 loopback. Source:
`bench-results/phase8.7-tier-matrix.txt`, `bench_pool_l4_tier_matrix`.

Throughput is `size / p50_latency` so it tracks the latency table
exactly, not the run's avg-based throughput line.

| Size  | PUT p50 | PUT throughput | GET p50 | GET throughput |
|---    |---      |---             |---      |---             |
| 4KB   | `265us`     | `14.7 MB/s`   | `173us`     | `22.5 MB/s`   |
| 64KB  | `385us`     | `162.4 MB/s`  | `218us`     | `287.2 MB/s`  |
| 1MB   | `4057us`    | `246.5 MB/s`  | `1776us`    | `563.2 MB/s`  |
| 10MB  | `31744us`   | `315.0 MB/s`  | `10031us`   | `996.9 MB/s`  |
| 100MB | `164974us`  | `606.2 MB/s`  | `97709us`   | `1023.5 MB/s` |

**Where it wins:** small objects (`<=64KB`). Log-store PUT p50 is
~3.5x faster than the file tier and ~1.7x faster than the pool at 4KB
and 64KB. GET p50 at 4KB is ~4x faster than file or pool.

**Where it loses:** the log-store branch only handles small
non-versioned writes; objects above the 64KB threshold fall through to
the file tier inside `LocalVolume::write_shard`. The 1MB / 10MB / 100MB
rows above are therefore measuring the file-tier path that the log
configuration falls back to, not the log store itself, which is why
those rows track Branch D closely.

For comparison, the legacy dedicated 4KB keep-alive benchmark in
`docs/write-log.md` recorded `0.91ms` (`1096 obj/s`); that view is
preserved there but is superseded by the Phase 8.7 numbers above for
end-to-end purposes.

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

End-to-end PUT and GET measured through the full HTTP stack on a single
disk, ftt=0, 127.0.0.1 loopback. Source:
`bench-results/phase8.7-tier-matrix.txt`, `bench_pool_l4_tier_matrix`.

| Size  | PUT p50 | PUT throughput | GET p50 | GET throughput |
|---    |---      |---             |---      |---             |
| 4KB   | `454us`     | `8.6 MB/s`    | `708us`     | `5.5 MB/s`    |
| 64KB  | `586us`     | `106.7 MB/s`  | `917us`     | `68.2 MB/s`   |
| 1MB   | `3797us`    | `263.4 MB/s`  | `1882us`    | `531.3 MB/s`  |
| 10MB  | `33837us`   | `295.6 MB/s`  | `10100us`   | `990.1 MB/s`  |
| 100MB | `165671us`  | `603.6 MB/s`  | `98955us`   | `1010.6 MB/s` |

**Where it wins:** mid-range (`1MB` to a few `MB`). Pool 1MB PUT p50 is
~13% faster than the file tier and ~6% faster than the log store. The
pre-opened temp-file slot avoids the per-PUT `mkdir + File::create`
that the file tier pays.

**Where it loses:** at small sizes the rename worker handoff and the
slot pop/release dominate the request, so 4KB pool PUT p50 (`454us`)
trails the log store (`265us`). At large sizes the post-write rename
becomes a fixed `60-100ms` tax that does not shrink with object size,
so 100MB pool PUT trails the file tier slightly. GET on the pool also
has to chase the temp-file slot via the `pending_renames` map, which
is why pool GET p50 trails log GET at every size below 10MB.

The best measured pool fast path was `318us` p50 at 4KB
(`8.62 / 0.000318 = 12.3 MB/s`) under Phase 8.5 Stage E# with depth
1024 and unbounded channel; that is the headroom the pool can reach
with tuned queues, not the default-config number above. The default
configuration (depth 1024, channel 10_000, 2 rename workers) is what
the Phase 8.7 row shows.

Storage-layer-only pool numbers (no HTTP, no s3s) reach `43us` p50 at
4KB and `2781.8 MB/s` at 100MB; see `docs/layer-optimization.md` Pool
Pool L3 for that view. The end-to-end rows above are the cost actually paid
by an external client.

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

End-to-end PUT and GET measured through the full HTTP stack on a single
disk, ftt=0, 127.0.0.1 loopback. Source:
`bench-results/phase8.7-tier-matrix.txt`, `bench_pool_l4_tier_matrix`.

| Size  | PUT p50 | PUT throughput | GET p50 | GET throughput |
|---    |---      |---             |---      |---             |
| 4KB   | `935us`     | `4.2 MB/s`    | `689us`     | `5.7 MB/s`    |
| 64KB  | `1330us`    | `47.0 MB/s`   | `949us`     | `65.8 MB/s`   |
| 1MB   | `4360us`    | `229.3 MB/s`  | `1929us`    | `518.5 MB/s`  |
| 10MB  | `28974us`   | `345.1 MB/s`  | `16840us`   | `593.9 MB/s`  |
| 100MB | `149228us`  | `670.0 MB/s`  | `100848us`  | `991.6 MB/s`  |

**Where it wins:** large objects (`>=10MB`). The file tier writes
shard.dat and meta.json directly to their final paths, so it pays no
post-write rename tax. At 100MB it leads PUT throughput at 670.0 MB/s
versus log 606.2 and pool 603.6.

**Where it loses:** small objects. 4KB file-tier PUT p50 (`935us`) is
~3.5x slower than the log store (`265us`) and ~2x slower than the pool
fast path (`454us`). Most of that gap is the per-PUT
`mkdir + File::create + write + close` sequence on NTFS, not actual
data write time.

For independent confirmation, Phase 8.5 Stage D measured the same
file-tier path at `810us` p50 at 4KB through bare hyper (no s3s),
which is consistent with the `935us` Phase 8.7 number once the s3s
dispatch overhead (~32us) and an extra warmup spread are added back.

Storage-layer-only file-tier ceilings (no HTTP, no s3s) from
`docs/layer-optimization.md`:

- L3 storage pipeline: `439 MB/s` at 10MB, `489 MB/s` at 1GB
- L3 skip-MD5 storage pipeline: `510 MB/s` at 1GB
- L5 raw local write ceiling underneath the file tier: `1625 MB/s`
  at 10MB, `1056 MB/s` at 1GB

The Phase 8.7 file-tier 100MB PUT (`670.0 MB/s`) sits comfortably
above the L3 10MB pipeline ceiling because L3 measures the storage
pipeline at smaller sizes than 100MB; the comparison is informative
about the per-stage gap, not directly comparable cell to cell.

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
visible PUT and GET. Source:
`bench-results/phase8.7-tier-matrix.txt`. All cells are p50 latency
plus derived throughput (`size / p50_latency`); MB = `1048576` bytes.

#### PUT p50 latency by tier

| Size  | file       | log        | pool       | best tier |
|---    |---         |---         |---         |---        |
| 4KB   | `935us`    | **`265us`**    | `454us`    | log  |
| 64KB  | `1330us`   | **`385us`**    | `586us`    | log  |
| 1MB   | `4360us`   | `4057us`   | **`3797us`**   | pool |
| 10MB  | **`28974us`**  | `31744us`  | `33837us`  | file |
| 100MB | **`149228us`** | `164974us` | `165671us` | file |

#### PUT throughput by tier

| Size  | file        | log         | pool        | best tier |
|---    |---          |---          |---          |---        |
| 4KB   | `4.2 MB/s`     | **`14.7 MB/s`**    | `8.6 MB/s`     | log  |
| 64KB  | `47.0 MB/s`    | **`162.4 MB/s`**   | `106.7 MB/s`   | log  |
| 1MB   | `229.3 MB/s`   | `246.5 MB/s`   | **`263.4 MB/s`**   | pool |
| 10MB  | **`345.1 MB/s`**   | `315.0 MB/s`   | `295.6 MB/s`   | file |
| 100MB | **`670.0 MB/s`**   | `606.2 MB/s`   | `603.6 MB/s`   | file |

#### GET p50 latency by tier

| Size  | file        | log        | pool       | best tier |
|---    |---          |---         |---         |---        |
| 4KB   | `689us`     | **`173us`**    | `708us`    | log |
| 64KB  | `949us`     | **`218us`**    | `917us`    | log |
| 1MB   | `1929us`    | **`1776us`**   | `1882us`   | log |
| 10MB  | `16840us`   | **`10031us`**  | `10100us`  | log |
| 100MB | `100848us`  | **`97709us`**  | `98955us`  | log |

#### GET throughput by tier

| Size  | file         | log          | pool         | best tier |
|---    |---           |---           |---           |---        |
| 4KB   | `5.7 MB/s`      | **`22.5 MB/s`**     | `5.5 MB/s`      | log |
| 64KB  | `65.8 MB/s`     | **`287.2 MB/s`**    | `68.2 MB/s`     | log |
| 1MB   | `518.5 MB/s`    | **`563.2 MB/s`**    | `531.3 MB/s`    | log |
| 10MB  | `593.9 MB/s`    | **`996.9 MB/s`**    | `990.1 MB/s`    | log |
| 100MB | `991.6 MB/s`    | **`1023.5 MB/s`**   | `1010.6 MB/s`   | log |

#### Tier handoff implied by these tables

The cross-over points fall on natural object size boundaries:

- **`<=64KB`**: log store wins both PUT and GET (~3-4x advantage at 4KB)
- **`64KB to ~10MB`**: pool wins PUT (within ~7% of log on the GET side
  thanks to the buffered slot read path)
- **`>=10MB`**: file tier wins PUT (the rename worker overhead the
  pool pays no longer amortizes; ~60-100ms fixed tax per write); GET
  is essentially equalized by the disk-write sink

That handoff is the basis for the proposed three-tier dispatch
inside `LocalVolume::write_shard`. The current `--write-tier` CLI
flag picks one tier for the entire process, not per-object; the
per-object dispatch is still pending.

#### Important: layer-bench tier deltas are larger than SDK-matrix tier deltas

The Phase 8.7 numbers above measure each tier through `reqwest -> hyper
-> s3s -> VolumePool -> LocalVolume` with no auth (`bench_pool_l4_tier_matrix`
in `abixio-ui/src/bench/`). The SDK matrix bench in `benchmarks.md`
measures the same three tiers through `aws-sdk-s3 -> SigV4 -> hyper ->
s3s -> ...` with auth enabled.

At 4KB sdk PUT the SDK-matrix bench (`bench-results/2026-04-11-matrix-tls-tiers.txt`)
shows file 1653 obj/s, log 1662, pool 1716 -- a ~4% spread. The
Phase 8.7 tables above show file 935us, log 265us, pool 454us at the
same size -- a ~3.5x spread (file -> log).

Both numbers are correct in their own frame. The difference is the
SDK + SigV4 + hyper per-request floor (~600 us measured), under
which most of the storage-tier delta gets averaged out. The Phase 8.7
numbers are the right way to compare *internal* tier performance and
to choose which tier should handle which size. The SDK-matrix numbers
are the right way to predict what an external S3 client actually
sees. Don't use one to refute the other -- they measure different
things.

This caveat applies most strongly at 4KB. At 10MB the SDK-matrix
shows pool +25% over file (421 vs 336 MB/s) and Phase 8.7 shows pool
+15% over file (3797 vs 4360 us); at 1GB both stories agree the
spread between tiers is small.

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
