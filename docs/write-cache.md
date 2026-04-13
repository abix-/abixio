# Write cache

**Authoritative for:** RAM write cache internals. DashMap data structure,
flush strategy, safety model, peer replication protocol design, and
implementation plan. If you need to know how the cache works internally,
this is the only doc.

**Not covered here:** how a PUT reaches the cache (see [write-path.md](write-path.md)),
optimization history (see [layer-optimization.md](layer-optimization.md)),
benchmark results (see [benchmarks.md](benchmarks.md)).

The cache acks from RAM with zero disk I/O on the write hot path.
Background flush destages to disk asynchronously. The current
implementation is local-only (single node DashMap). Peer replication
sections below are design direction, not implemented behavior.

## Why

Even the log-structured storage path writes to disk. On NTFS, the fastest
possible disk write (append to pre-allocated file, no fsync) takes 0.002ms.
But the full server path through HTTP/s3s adds 0.9ms. The disk write itself
is cheap, but every write still touches the filesystem.

The fix is to skip the disk entirely on the write path: write to RAM,
replicate to a peer's RAM, ack. The disk write happens in the background,
100ms later. Write latency drops from milliseconds to microseconds.
Older write-back caching products from the enterprise virtualization era
shipped this model in production for years; the safety analysis below is
the same one they relied on.

## Architecture

This section is the design intent. The current implementation is a
smaller subset:

- local RAM cache insert on the small-object path
- read hits from RAM cache
- explicit flush to lower storage tiers through `flush_write_cache`

The current implementation does **not** yet wire the peer replication
endpoint or a periodic background destage worker from `main.rs`.

```
PUT 4KB (2+ node cluster):

  Client request
    |
    v
  S3 parse + RS encode                (~0.05ms)
    |
    v
  Insert into LOCAL RAM cache          (~0.001ms, DashMap)
    |
    +---> Replicate to PEER RAM        (~0.05-0.1ms, LAN HTTP POST)
    |     Peer inserts into its RAM    |
    |     Peer responds 200 OK  <------+
    |
    v
  Ack to client                        total server: ~0.1ms
    |
    v (async, ~100ms later)
  Background flush to disk             (WAL / file tier)
```

```
GET 4KB:

  Client request
    |
    v
  Check local RAM cache               (~0.001ms, DashMap lookup)
    |
    +---> HIT: return from RAM         (zero disk I/O)
    |
    +---> MISS: check WAL pending -> file tier -> disk read
```

## Safety model

**Requirement: minimum 2 nodes.** Single-node clusters cannot use the
write cache. They fall through to the existing disk write path.

| Failure scenario | Data safe? | Recovery |
|-----------------|-----------|----------|
| Node A process crash | YES | Peer B has data in RAM, flushes to disk |
| Node A power loss | YES | Peer B still running (separate power) |
| Node A crash + restart | YES | Peer B re-sends cached entries to A |
| Both nodes crash | NO | Unflushed data lost (requires simultaneous failure) |
| Slow flush (>100ms) | YES | Both nodes have data in RAM during flush window |

The probability of two independent node failures within the flush
interval (~100ms) is negligible. This safety model is the same one
older write-back caching products relied on in enterprise production.

## Peer replication protocol

New internode endpoint on the existing storage server:

```
POST /_cache/v1/replicate
Headers:
  X-Cache-Bucket: mybucket
  X-Cache-Key: photos/cat.jpg
  X-Cache-Size: 4096
  Authorization: Bearer <JWT>     (existing internode auth)
Body: msgpack-serialized CacheEntry (shards + metadata)

Response: 200 OK                  (data in peer RAM)
```

Uses persistent HTTP connection between nodes (keep-alive). The peer
inserts into its own DashMap and responds. No disk write on the peer.
Pure RAM on both sides.

## When the RAM cache wins

1. **Concurrent requests**: `DashMap` is lock-free. The WAL uses
   `Mutex`. Under concurrent connections, `DashMap` scales linearly
   while the WAL mutex serializes.

2. **Peer replication** (future): removes disk from the write path
   entirely. Storage cost is RAM insert (local) + RAM insert (peer).

3. **Non-HTTP protocols** (future): a binary protocol could expose the
   sub-microsecond `DashMap.insert` latency directly.

For end-to-end numbers, see [write-path.md](write-path.md) and
[benchmarks.md](benchmarks.md).

## Data structure

```rust
struct WriteCache {
    /// (bucket, key) -> CacheEntry
    entries: DashMap<(Arc<str>, Arc<str>), CacheEntry>,
    /// total bytes in cache
    size_bytes: AtomicU64,
    /// configurable max size (default 256MB)
    max_bytes: u64,
}

struct CacheEntry {
    shards: Vec<Vec<u8>>,        // RS-encoded shard data
    metas: Vec<ObjectMeta>,      // per-shard metadata
    distribution: Vec<usize>,    // shard -> disk mapping
    original_size: u64,
    etag: String,
    cached_at: Instant,
    replicated: bool,            // peer confirmed?
}
```

`DashMap` for lock-free concurrent access. No mutex on the hot path.
GET and PUT run concurrently without blocking.

256MB cache = ~50K objects at 4KB each (post-RS-encode, 4 shards x 1.3KB).

## Background flush

Each node runs an independent flush task:

```
loop every 100ms:
  for each cached entry older than min_age (10ms):
    write each shard to the appropriate disk (WAL or file tier)
    if all shards written: remove from local cache
    // peer flushes independently on its own schedule
```

After flush: object is on disk. Cache entry removed. Peer cache entry
cleared independently when the peer flushes.

## Recovery

### Node restart (after crash)

1. Restarted node comes up with empty RAM cache
2. Peer node detects the restart
3. Peer sends unflushed entries that belong to the restarted node
   via `/_cache/v1/sync` (batch transfer)
4. Restarted node flushes received entries to disk
5. Normal operation resumes

### Cache full

When `size_bytes >= max_bytes`:
- New writes bypass the cache and go directly to disk (existing path)
- Background flush continues to drain the cache
- Once space is available, new writes use the cache again

No blocking. No backpressure to clients. Graceful degradation.

## Configuration

```
--write-cache-size 256MB    # 0 = disabled. auto-enables with 2+ nodes
--write-cache-flush-ms 100  # background flush interval
```

## Implementation plan

| Phase | What | Delivers |
|-------|------|----------|
| 1 | `write_cache.rs`: DashMap, insert/get/remove, size tracking | core |
| 2 | Wire PUT: encode -> cache insert -> ack (local only) | local RAM writes |
| 3 | Wire GET: cache lookup before disk | RAM read hits |
| 4 | Background flush task | data reaches disk |
| 5 | Peer replication (HTTP POST to peer) | durability |
| 6 | `/_cache/v1/replicate` endpoint | peer receives |
| 7 | PUT: require peer ack before client ack | full peer-replicated mode |
| 8 | Recovery: peer re-flush on node restart | crash safety |

MVP = phases 1-4 (local RAM, single node, proves speed).
Full peer-replicated mode = phases 5-8 (peer replication, 2+ nodes,
crash safe).

For how the cache fits into the overall write and read paths, see
[write-path.md](write-path.md).

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| `VolumePool` can enable a RAM write cache and insert small buffered objects into it before disk writes | Verified | `src/storage/volume_pool.rs`, `src/storage/write_cache.rs`, `src/main.rs` |
| Cache hits are visible to the read path before disk persistence | Verified | `src/storage/volume_pool.rs` read paths consult `write_cache()` before disk backends |
| The current cache implementation is local-only | Verified | `src/storage/write_cache.rs`, `src/main.rs` |
| `flush_write_cache()` exists and writes cached shards out through backend `write_shard` calls | Verified | `src/storage/volume_pool.rs`, `src/admin/handlers.rs` |
| A periodic background flush task is wired in the current runtime | Not implemented in current wiring | No cache-destage task is spawned in `src/main.rs`; flush is explicit via `flush_write_cache()` |
| Peer replication protocol and `/_cache/v1/*` endpoints are implemented | Not implemented in current code | No cache replication server routes exist in `src/storage/storage_server.rs` |
| The timing tables in this doc are current product-level measurements | Needs nuance | Some timings are useful design references, but much of this doc mixes measured internals with forward-looking design assumptions |

Verdict: the in-memory cache data structure and manual flush path are real. The peer-replicated write-back design remains partly aspirational and should be read as design direction unless this report says otherwise.
