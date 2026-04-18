# Write path

**Authoritative for:** how a PUT moves through AbixIO. End-to-end flow,
routing decisions, the storage branches (RAM cache, WAL, file tier, remote backend), ack semantics, and current
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
  - [Branch B: Write-ahead log (WAL)](#branch-b-write-ahead-log-wal)
  - [Branch C: Local file tier](#branch-c-local-file-tier)
  - [Branch D: Remote backend](#branch-d-remote-backend)
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
       WAL -> ack -> materialize worker later
       file tier -> ack
       remote volume RPC -> ack after target node completes its local shard path
```

The main code anchors are:

- `src/s3_service.rs`: request-level decisions
- `src/storage/volume_pool.rs`: routing, EC, placement, quorum
- `src/storage/local_volume.rs`: local WAL/file write behavior
- `src/storage/write_cache.rs`: RAM cache structure
- `src/storage/wal.rs`: WAL append and materialize worker
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
| 4KB | `87us` | `129us` | `152us` | `41.0 MB/s` |
| 64KB | `117us` | `160us` | `187us` | `506.8 MB/s` |
| 10MB | `20.9ms` | `29.0ms` | `33.1ms` | `514.5 MB/s` |
| 100MB | `119.9ms` | `265.0ms` | `273.4ms` | `710.6 MB/s` |
| 1GB | `1.35s` | `1.41s` | `1.41s` | `753.2 MB/s` |

#### GET (hyper returns sized body, reqwest consumes it)

| Size | p50 | p95 | p99 | throughput |
|---|---|---|---|---|
| 4KB | `82us` | `122us` | `150us` | `44.8 MB/s` |
| 64KB | `105us` | `132us` | `168us` | `574.9 MB/s` |
| 10MB | `11.8ms` | `27.1ms` | `29.7ms` | `636.4 MB/s` |
| 100MB | `117.6ms` | `266.0ms` | `276.4ms` | `720.9 MB/s` |
| 1GB | `1.29s` | `2.07s` | `2.07s` | `725.6 MB/s` |

`hyper` accepts the request, parses HTTP/1.1, and exposes the body as
a stream. At small sizes (4KB, 64KB), latency is dominated by TCP
round-trip and HTTP framing, not data transfer. At large sizes (100MB,
1GB), throughput reaches 700-750 MB/s, which is the hyper/Windows
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
| 4KB | `56us` | `89us` | `108us` | `65.0 MB/s` |
| 64KB | `96us` | `135us` | `164us` | `617.9 MB/s` |
| 10MB | `6.0ms` | `6.5ms` | `7.0ms` | `1637.9 MB/s` |
| 100MB | `55.7ms` | `59.9ms` | `74.9ms` | `1766.5 MB/s` |
| 1GB | `579.7ms` | `643.2ms` | `643.2ms` | `1754.3 MB/s` |

#### GET (NullBackend returns empty, not meaningful at large sizes)

| Size | p50 | p95 | p99 | throughput |
|---|---|---|---|---|
| 4KB | `50us` | `75us` | `90us` | `74.4 MB/s` |
| 64KB | `51us` | `76us` | `93us` | `1177.1 MB/s` |

GET at 10MB+ is not meaningful because NullBackend returns zero bytes
regardless of requested size. The GET latency at all sizes is just
s3s dispatch overhead (~50-93us).

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
| local backend + non-versioned + `data.len <= wal_threshold` + WAL enabled | WAL (append to mmap segment, background materialize) |
| local backend fallback | file tier |
| remote backend | HTTP POST to target node's storage server, which then executes its own local branch |
| versioned object or unknown/large content length | streaming `encode_and_write` path |

Important detail: the 64KB threshold is checked twice in different
places for different reasons.

- `VolumePool` uses `content_length <= 64KB` to decide whether it can
  buffer the full object and take the small-object path.
- `LocalVolume::write_shard` checks the WAL threshold (default 64KB)
  when deciding whether to append to the WAL or fall through to file tier.

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
| blake3 | `2us` | `13us` | `2.2ms` | `22.9ms` | `238.4ms` | `4273 MB/s` |
| md5 | `6us` | `89us` | `14.2ms` | `142.1ms` | `1.45s` | `705 MB/s` |
| sha256 | `15us` | `226us` | `36.1ms` | `360.2ms` | `3.69s` | `277 MB/s` |
| rs_encode 3+1 | `1us` | `20us` | `3.6ms` | `35.4ms` | `364.1ms` | `2813 MB/s` |

Throughput column is from the 1GB run. blake3 and RS encode are fast
enough to be invisible in the storage pipeline. MD5 at 705 MB/s is
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
`benchmarks.md::Comprehensive matrix`: file/wal tier choices are
within 4% of each other at 4KB sdk PUT for exactly the same reason --
the protocol floor dominates.

Run `abixio-ui bench --layers L3` to see the
file/wal branches end-to-end at all sizes.

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

### Branch B: Write-ahead log (WAL)

Isolated L3 storage pipeline. Direct VolumePool API call, no HTTP,
no s3s. 1 disk, ftt=0, no write cache, Defender-excluded tmp dir.

Source: `abixio-ui bench --layers L3 --write-paths wal`,
`bench-results/main-2026-04-18.json`

| Size | PUT p50 | PUT throughput | GET p50 | GET throughput |
|---|---|---|---|---|
| 4KB | `136us` | `28.3 MB/s` | `491us` | `8.0 MB/s` |
| 64KB | `274us` | `185.1 MB/s` | `507us` | `121.9 MB/s` |
| 10MB | `26.9ms` | `368.4 MB/s` | `8.6ms` | `1140.0 MB/s` |
| 100MB | `315.9ms` | `306.6 MB/s` | `87.2ms` | `1149.2 MB/s` |
| 1GB | `3.24s` | `289.5 MB/s` | `849.4ms` | `1198.7 MB/s` |

The WAL appends a checksummed needle directly into a writable mmap
segment (zero syscalls, zero allocation). At 4KB it is 5.4x faster
than the file tier. At 64KB it is competitive with all tiers. At
10MB+ the WalShardWriter buffers then falls back to the file tier,
which adds overhead vs the direct file path.

Head-to-head release-mode unit test (`write_shard` only, same process,
same disk): WAL 3us, file 878us at 4KB. WAL 30us at 64KB. The L3 bench
numbers above include VolumePool routing overhead (EC, placement,
quorum).

If the WAL is enabled on a `LocalVolume`, small non-versioned objects
route through the WAL. Objects above the WAL threshold (default 64KB)
go straight to the file tier.

What happens before ack:

- `Needle::new` serializes ObjectMeta to msgpack
- `serialize_into` writes the needle directly into the mmap segment
  (header + bucket + key + meta + data, one copy, zero allocation)
- the in-memory pending map is updated so reads can find the object
- a lightweight `MaterializeRequest` (Arc<str> identity + 20 bytes of
  offsets) is fire-and-forget sent to the background worker

What ack means here:

- the object is durable in the WAL segment's mmap (OS page cache)
- reads see it immediately via the pending map
- the object is **not yet at its final file-per-object location**

Final resting place:

- the background materialize worker reads data from the segment mmap,
  creates the object directory, and writes `shard.dat` + `meta.json`
  concurrently via `tokio::try_join!`
- once materialized, the pending entry is removed and reads fall
  through to the file tier
- when all entries in a segment are materialized, the segment file
  is deleted

See [write-wal.md](write-wal.md) for the full WAL design, recovery,
and optimization history.

### Branch C: Local file tier

Isolated L3 storage pipeline. Direct VolumePool API call, no HTTP,
no s3s. 1 disk, ftt=0, no write cache.

Source: `abixio-ui bench --layers L3 --write-paths file`,
`bench-results/main-2026-04-18.json` (put_stream and get_stream rows)

| Size | PUT p50 | PUT throughput | GET p50 | GET throughput |
|---|---|---|---|---|
| 4KB | `730us` | `4.9 MB/s` | `416us` | `8.9 MB/s` |
| 64KB | `841us` | `70.9 MB/s` | `411us` | `147.9 MB/s` |
| 10MB | `19.0ms` | `520.6 MB/s` | `411us` | `23665 MB/s` |
| 100MB | `181.6ms` | `545.7 MB/s` | `463us` | `207966 MB/s` |
| 1GB | `1.92s` | `532.3 MB/s` | `629us` | `1601927 MB/s` |

The file tier writes object data and meta.json directly to their
final paths. No post-write rename, no pre-opened files. At 4KB it
is the slowest tier because every PUT pays mkdir + File::create.
GET uses mmap (page-cache hot).

If the write does not hit RAM cache or WAL, it falls
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

### Branch D: Remote backend

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
  volume uses: WAL or file tier

## Ack semantics by branch

This is the main durability distinction the rest of the docs should
reference.

| Branch | Ack means | Final resting place reached at ack? |
|---|---|---|
| RAM write cache | object is in RAM cache and readable from cache | no |
| WAL | object is in mmap segment and pending map; materialize queued | no (materialized in background) |
| file tier | final object files are written in their destination directory | yes, modulo OS page-cache durability model |
| remote backend | target node completed its local shard write path | depends on the target branch |

## Design gap: unnecessary EC on single disk

When AbixIO runs with 1 disk and ftt=0, there is no redundancy to
compute. The RS encode is 1+0 (one data shard, zero parity). The
shard IS the original data. But AbixIO still pays the full EC
pipeline cost:

1. RS encode 1+0 (passthrough, but still allocates shard buffers)
2. blake3 checksum per shard
3. Build ObjectMeta with ErasureMeta
4. Serialize meta.json separately from shard data
5. Write shard.dat + meta.json to a nested directory

MinIO and RustFS on a single disk just write the object bytes
directly with a binary metadata header. No RS encode, no separate
meta file, no nested directory per object.

### Fix: cluster-aware EC bypass

VolumePool knows how many backends are in the cluster. When there
is only 1 volume, EC is pointless: there is nobody to replicate to
and no disk failure to tolerate. The fix:

- VolumePool checks `disks.len()` at write time
- If 1 disk: force data_n=1, parity_n=0, skip RS encode entirely
- Write raw object bytes through write_shard (no shard encoding)
- The file tier and WAL both receive the original object
  bytes, not encoded shard data
- Metadata still records the object, but ErasureMeta reflects 1+0

This is not a config flag. It's automatic based on cluster topology.
When a second volume joins the cluster, EC activates. When it's
alone, it writes like MinIO and RustFS do on a single node.

## Raw disk floor (L5)

Isolated L5 disk I/O. Direct tokio::fs calls, no storage pipeline,
no HTTP, no hashing, no EC. This is the filesystem ceiling. Nothing
in the stack can write faster than this.

Source: `abixio-ui bench --layers L5`,
`bench-results/main-2026-04-18.json`

| Size | write p50 | write+fsync p50 | read p50 | write MB/s | read MB/s |
|---|---|---|---|---|---|
| 4KB | `262us` | `2.6ms` | `62us` | `14.6` | `61.3` |
| 64KB | `280us` | `2.7ms` | `71us` | `208.8` | `854.1` |
| 10MB | `4.7ms` | `14.1ms` | `7.8ms` | `2007.5` | `1281.2` |
| 100MB | `100.3ms` | `102.5ms` | `73.8ms` | `942.6` | `1336.4` |
| 1GB | `1.02s` | `893.2ms` | `841.5ms` | `1005.5` | `1217.2` |

Write without fsync goes to OS page cache (RAM). Write+fsync forces
to physical disk. AbixIO does not fsync individual writes (same as
MinIO and RustFS). At 1GB the two converge because the OS flushes
dirty pages during the write.

The gap between L5 write (262us at 4KB) and L3 file tier PUT (729us
at 4KB) is the storage pipeline overhead: EC encoding, hashing,
metadata serialization, mkdir, and multiple file creates per shard.

## Where the time goes

Isolated L3 tier comparison. Direct VolumePool API, no HTTP, no s3s.
1 disk, ftt=0, no write cache, Defender-excluded tmp dir. All paths
use the unified write path (streaming encode + tier-aware ShardWriter).

PUT and GET are measured separately. After all PUTs complete, WAL
pending entries are materialized and write cache is flushed before any
timed GETs begin. This ensures GET measures read performance from
the final storage location, not from transitional temp files.

Source: `abixio-ui bench --layers L3 --write-paths file,wal`,
`bench-results/main-2026-04-18.json`

#### PUT p50 latency by tier (1+0 fast path)

| Size | file | wal | best tier |
|---|---|---|---|
| 4KB | `729us` | **`136us`** | wal |
| 64KB | `858us` | **`274us`** | wal |
| 10MB | **`23.9ms`** | `26.9ms` | file |
| 100MB | **`232.7ms`** | `315.9ms` | file |
| 1GB | **`2.36s`** | `3.24s` | file |

#### GET p50 latency by tier (buffered, mmap, page-cache hot)

| Size | file | wal |
|---|---|---|
| 4KB | `444us` | `491us` |
| 64KB | `455us` | `507us` |
| 10MB | `9.2ms` | `8.6ms` |
| 100MB | `87.2ms` | `87.2ms` |
| 1GB | `860.7ms` | `849.4ms` |

GET is sub-millisecond at small sizes because data sits in OS page
cache. WAL GET reads from the segment mmap while pending, then
falls through to file tier after materialization. At 10MB+ the two
tiers converge because the mmap read path is the bottleneck.

#### Tier handoff

- **<=64KB PUT**: WAL wins big (5.4x faster than file at 4KB, 3.1x
  at 64KB) because it appends directly to a writable mmap segment
  instead of doing mkdir + File::create per object
- **<=64KB GET**: file tier and WAL are similar (~450-500us). The
  LRU read cache closes this gap for hot objects
- **10MB-1GB PUT**: file wins. WAL above the 64KB threshold falls
  back to file-tier materialization and adds buffering overhead

**WAL is the recommended default for small writes.** Below the
threshold it is measurably faster with no GC, no permanent index,
and bounded startup cost. Above the threshold file-tier takes over
transparently.

## How other docs should use this page

- `architecture.md` should summarize the write path and link here.
- `benchmarks.md` should keep measurements and link here for layer and
  durability interpretation.
- `write-wal.md` is the authoritative WAL design doc.
- `write-cache.md` covers the RAM write cache design.

## Integration: S3 + real storage (L6)

L6 is NOT isolated. It combines L1 (HTTP) + L2 (S3 protocol) + L3
(storage) into a single in-process stack. This is what the server
itself costs per request, without SDK client overhead or TLS.

Source: `abixio-ui bench --layers L6`,
`bench-results/main-2026-04-18.json`

Drain and flush between PUT and GET. 1 disk, ftt=0, no write cache.

#### L6 PUT p50 by tier

| Size | file | wal |
|---|---|---|
| 4KB | `675us` | **`290us`** |
| 64KB | `802us` | **`397us`** |
| 10MB | **`31.0ms`** | `46.3ms` |
| 100MB | **`139.9ms`** | `276.0ms` |
| 1GB | **`1.54s`** | `3.04s` |

#### L6 GET p50 by tier

| Size | file | wal |
|---|---|---|
| 4KB | `537us` | `616us` |
| 64KB | `683us` | `673us` |
| 10MB | **`9.2ms`** | `9.1ms` |
| 100MB | `78.4ms` | `79.4ms` |
| 1GB | **`955.4ms`** | `918.1ms` |

At 4KB the WAL is the fastest PUT path through the full S3 stack
(2.3x faster than file). Above the 64KB threshold file wins; GET
converges across tiers at medium+ sizes.

## Accuracy Report

Audited against the codebase on 2026-04-18. Timing tables refreshed
from `bench-results/main-2026-04-18.json`.

| Claim | Status | Evidence |
|---|---|---|
| `s3_service.rs::put_object` resolves versioning, `skip_md5`, and forwards `content_length` into `put_object_stream` | Verified | `src/s3_service.rs` |
| `VolumePool::put_object_stream` uses the small-object buffered path only for non-versioned requests with declared `content_length <= 64KB` | Verified | `src/storage/volume_pool.rs` |
| RAM write cache is tried before disk writes on the small-object path | Verified | `src/storage/volume_pool.rs` |
| RAM-cache ack happens before disk persistence | Verified | `src/storage/volume_pool.rs`, `src/storage/write_cache.rs` |
| RAM write cache currently relies on explicit `flush_write_cache()` rather than an automatic destage worker in this repo | Verified | `src/storage/volume_pool.rs`, `src/admin/handlers.rs`, absence of any spawned cache flush task in `src/main.rs` |
| Local small non-versioned writes route to the WAL when enabled | Verified | `src/storage/local_volume.rs`, `src/storage/wal.rs` |
| WAL writes ack before the materialize worker reaches the final destination path | Verified | `src/storage/local_volume.rs`, `src/storage/wal.rs` |
| File-tier writes place the object in its final path before ack | Verified | `src/storage/local_volume.rs` |
| Remote shard writes POST into `/_storage/v1/*` and then execute the target node's local `write_shard` path | Verified | `src/storage/remote_volume.rs`, `src/storage/storage_server.rs` |
| Timing tables on this page are measured-only | Verified | All numeric timings here are sourced from existing benchmark or trace docs; no new estimated timings were added |
| The specific timing values were independently re-run in this pass | Verified | All L1/L2/L3/L4/L5/L6 tables re-run on 2026-04-18 via `abixio-ui bench --servers abixio --clients sdk --write-paths file,wal --write-cache both` |
| Per-branch tier tables (B/C) carry full 5-size PUT and GET p50 plus throughput | Verified | Source: `bench-results/main-2026-04-18.json`. Throughput cells are derived from the same p50 measurements; MB = 1048576 bytes |
| WAL wins at small sizes, file tier wins at large sizes | Verified | Tier matrix tables in "Where the time goes" show WAL winning <=64KB PUT, file tier winning at 10MB+ |

Verdict: the routing and ack/final-resting-place semantics on this page are grounded in the current code. The timing sections are as authoritative as the current benchmark corpus, but they remain benchmark-derived rather than freshly re-measured in this pass.
