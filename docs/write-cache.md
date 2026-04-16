# Write cache

**Authoritative for:** RAM write cache internals. DashMap data structure,
flush strategy, safety model, peer replication protocol design, and
implementation plan. If you need to know how the cache works internally,
this is the only doc.

**Not covered here:** how a PUT reaches the cache (see [write-path.md](write-path.md)),
optimization history (see [layer-optimization.md](layer-optimization.md)),
benchmark results (see [benchmarks.md](benchmarks.md)).

The cache acks from RAM with zero disk I/O on the write hot path.
In multi-node mode the cache replicates each insert to one peer before
acking so a single node crash cannot lose the entry. A background
flush task destages entries older than `--write-cache-min-age-ms` to
disk on the primary; peers hold replicas in RAM but never flush them
themselves (their `distribution` indexes point into the primary's
VolumePool, not their own).

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

Two internode endpoints on the existing storage server, sharing the
JWT auth used by every other `_storage/v1/*` route:

```
POST /_storage/v1/cache-replicate?bucket=B&key=K
Headers:
  Authorization: Bearer <JWT>
  X-Abixio-Time: <unix-nanos>
  Content-Type: application/msgpack
Body: msgpack-serialized CacheEntry (shards + metadata + primary_node_id)

Response: 200 OK            (inserted into peer's WriteCache)
         503                 (peer has no write cache configured)
         507 (InsufficientStorage)  (peer cache full)
```

```
GET /_storage/v1/cache-sync?primary_node_id=<node-id>
Headers:
  Authorization: Bearer <JWT>
  X-Abixio-Time: <unix-nanos>

Response: 200 OK
  Content-Type: application/msgpack
  Body: Vec<(bucket, key, CacheEntry)>   (drained from peer)
```

Reqwest keep-alive pools connections per peer. Peer only inserts into
its own `DashMap` -- no disk write on the peer side. Pure RAM on both
sides of a normal replicate.

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
    shards: Vec<bytes::Bytes>,   // RS-encoded shard data (ref-counted)
    metas: Vec<ObjectMeta>,      // per-shard metadata
    distribution: Vec<usize>,    // shard -> disk mapping (primary's indices)
    original_size: u64,
    etag: String,
    cached_at_unix_nanos: u64,   // wire-portable timestamp
    cached_at_local: Instant,    // node-local age comparator (not serialized)
    primary_node_id: String,     // who owns and flushes this entry
}
```

`DashMap` for lock-free concurrent access. No mutex on the hot path.
GET and PUT run concurrently without blocking.

256MB cache = ~50K objects at 4KB each (post-RS-encode, 4 shards x 1.3KB).

## Background flush

Each node runs an independent flush task, spawned from `main.rs`:

```
loop every --write-cache-flush-ms (default 100ms):
  drain cache entries where:
    - cached_at is older than --write-cache-min-age-ms (default 10ms)
    - primary_node_id matches this node
  for each drained entry:
    for each shard in shards:
      write_shard to the disk indexed by distribution[i]
```

Only the primary flushes its own entries. Peers keep replicas in RAM
for crash-fallback and drop them when the primary pulls them back
during restart sync or when the entry is overwritten by a subsequent
replicate. Peers never call `write_shard` on replicated entries.

## Recovery

### Node restart (after crash)

1. Restarted node comes up with empty RAM cache.
2. On startup, `VolumePool::recover_cache_from_peers` calls each peer's
   `GET /_storage/v1/cache-sync?primary_node_id=<self>` endpoint.
3. Each peer returns (and drops) every entry it holds where
   `primary_node_id` matches the caller. Transport: msgpack body.
4. Restarted node inserts the recovered entries into its local cache
   with fresh `cached_at_local` timestamps.
5. The normal background flush task destages them to local disk at the
   usual cadence.

If no peers hold replicas (single-node deployment, or all peers were
also down), the recovery call is a no-op and any unflushed entries
from before the crash are simply lost. The node must treat its disks
as the only source of truth in that case.

### Cache full

When `size_bytes >= max_bytes`:
- New writes bypass the cache and go directly to disk (existing path)
- Background flush continues to drain the cache
- Once space is available, new writes use the cache again

No blocking. No backpressure to clients. Graceful degradation.

## Configuration

```
--write-cache <MB>                   # RAM cache size. 0 disables. Default 256.
--write-cache-flush-ms <ms>          # background flush interval. Default 100.
--write-cache-min-age-ms <ms>        # minimum entry age before flush. Default 10.
--write-cache-peer-replicate <bool>  # replicate each insert to one peer
                                       # before acking. Default true. Ignored
                                       # when no peers are configured.
```

## Implementation status

All phases 1-8 are implemented as of 2026-04-16. The cache ships full
peer-replicated mode: local RAM insert, GET hits, background flush on
primary, synchronous replicate-to-one-peer before client ack, and
pull-based restart recovery from peers. Single-peer replication is the
MVP; N-way replication is a future extension gated on concurrent-client
benchmark data (kovarex #5).

For how the cache fits into the overall write and read paths, see
[write-path.md](write-path.md).

## Accuracy Report

Audited against the codebase on 2026-04-16.

| Claim | Status | Evidence |
|---|---|---|
| `VolumePool` can enable a RAM write cache and insert small buffered objects into it before disk writes | Verified | `src/storage/volume_pool.rs`, `src/storage/write_cache.rs`, `src/main.rs` |
| Cache hits are visible to the read path before disk persistence | Verified | `src/storage/volume_pool.rs` read paths consult `write_cache()` before disk backends |
| Periodic background flush task is wired | Verified | `src/main.rs` spawns the flush task when `--write-cache > 0` and `--write-cache-flush-ms > 0` |
| Cache entries carry a `primary_node_id` and only the primary flushes them | Verified | `src/storage/write_cache.rs::drain_primary_older_than`, `src/storage/volume_pool.rs::flush_older_than` |
| Peer replication protocol and `/_storage/v1/cache-*` endpoints are implemented | Verified | `src/storage/storage_server.rs::handle_cache_replicate`, `handle_cache_sync`; `src/storage/peer_cache.rs` |
| PUT awaits a single-peer replica before acking the client | Verified | `src/storage/volume_pool.rs` small-object path calls `peer.replicate` before `cache.insert` |
| On startup each node pulls entries it is primary for from peers | Verified | `src/storage/volume_pool.rs::recover_cache_from_peers`, spawned from `src/main.rs` |
| Single-peer replication is the only mode shipped | Verified | First client in `peer_cache_clients` is used; N-way is explicitly a follow-up |
| Versioned / multipart objects go through the cache | No | Only the small-object buffered path at `volume_pool.rs:put_object_stream` inserts into cache; streaming large objects and versioned writes skip it |
| Cache metrics and histograms exist | No | Observability is tracked as kovarex #8 |

Verdict: the cache implements the full peer-replicated write-back design end-to-end for the small-object buffered PUT path. Large/streaming/versioned objects still bypass it by construction.
