# Write cache

RAM write cache with peer replication. Writes go to RAM on two nodes
simultaneously, ack after both confirm. Background flush destages to
disk asynchronously. Both nodes must fail simultaneously to lose data.

The design comes from our own latency measurements (see request trace
below): the disk write itself is microseconds, but every write through
the file tier still touches the filesystem. Acking from RAM with peer
replication for durability removes the filesystem from the hot path
entirely.

## Why

Even the log-structured storage path writes to disk. On NTFS, the fastest
possible disk write (append to pre-allocated file, no fsync) takes 0.002ms.
But the full server path through HTTP/s3s adds 0.9ms. The disk write itself
is cheap -- but every write still touches the filesystem.

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
  Background flush to disk             (log store / file tier)
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
    +---> MISS: check log store -> file tier -> disk read
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

## Request trace: where every microsecond goes

### 4KB PUT through RAM cache

```
Client sends HTTP PUT with 4KB body
                    |
[0.08ms]  hyper: accept TCP, parse HTTP/1.1 headers, read body
                    |
[0.10ms]  s3s: extract S3 headers, dispatch to AbixioS3::put_object()
                    |
[0.05ms]  s3_service: check versioning config (cached HashMap),
          extract content_type, metadata, ec-ftt
                    |
[0.02ms]  volume_pool: content_length <= 64KB -> small object path
          collect body into Vec<u8>
                    |
[0.03ms]  RS encode: split_data + parity, MD5 ETag, blake3 checksums
                    |
[0.01ms]  placement: hash key -> pick disk for each shard
                    |
[0.001ms] >>> DashMap.insert(key, CacheEntry) <<<  THE ACTUAL STORAGE
                    |
[0.08ms]  s3s: serialize response, hyper writes to TCP
                    |
          Client receives 200 OK

          Storage:  0.001ms  (0.3% of total)
          HTTP/s3s: 0.39ms   (99.7% of total)
```

### 4KB GET from RAM cache

```
Client sends HTTP GET
                    |
[0.08ms]  hyper: parse request
                    |
[0.10ms]  s3s: dispatch
                    |
[0.001ms] >>> DashMap.get(key) -> clone Bytes refs (zero-copy) <<<
          reassemble shards: shard[0]+shard[1]+shard[2] = 4096 bytes
                    |
[0.08ms]  s3s: serialize response + body to TCP
                    |
          Client receives 200 OK + 4KB

          Storage:  0.001ms  (0.3% of total)
          HTTP/s3s: 0.26ms   (99.7% of total)
```

### Why the cache benchmarks the same as disk

The DashMap operation is 1 microsecond. The HTTP/s3s overhead is 300
microseconds. The storage tier is invisible under the protocol floor:

```
                    storage      HTTP/s3s     total
RAM cache:          0.001ms  +   0.3ms    =   0.3ms
Log store:          0.05ms   +   0.3ms    =   0.35ms
File tier:          0.2ms    +   0.3ms    =   0.5ms
```

All three benchmarked within noise (~1100-1350 obj/s) because the 0.3ms
HTTP floor dominates.

### When the cache wins

1. **Concurrent requests**: DashMap is lock-free. The log store uses
   `Mutex`. Under 100 concurrent connections, DashMap scales linearly
   while Mutex serializes.

2. **Peer replication** (future): the disk write is removed entirely.
   The only storage cost is RAM insert (local) + RAM insert (peer over
   LAN). Both sub-millisecond.

3. **Non-HTTP protocols** (future): a binary protocol could expose the
   1-microsecond storage latency directly, bypassing the 0.3ms floor.

## Performance

### Measured NTFS operation costs (baseline)

```
mkdir + shard.dat + meta.json:   0.628ms  1593 obj/s   (file tier)
mkdir + meta.json (inline):      0.435ms  2301 obj/s   (inline)
log store append:                0.002ms  544K obj/s   (log store)
DashMap insert:                  0.001ms  1M+ obj/s    (RAM cache)
```

### Expected end-to-end (4KB PUT, keep-alive)

| Path | Server processing | Total (inc. HTTP) | obj/sec |
|------|------------------|-------------------|---------|
| File tier (current) | 1.33ms | 1.73ms | 578 |
| Log store | 0.53ms | 0.91ms | 1118 |
| **RAM cache (local)** | **~0.05ms** | **~0.4ms** | **~2500** |
| **RAM + peer (1 GbE)** | **~0.1ms** | **~0.45ms** | **~2200** |
| **RAM + peer (10 GbE)** | **~0.06ms** | **~0.41ms** | **~2400** |

### Peer replication latency

```
4KB wire time:     32us (1 GbE) / 3us (10 GbE)
LAN RTT:           50-100us (same switch)
HTTP overhead:     ~50us (keep-alive internode connection)
Total:             ~50-100us per replication
```

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
    write each shard to the appropriate disk (log store or file tier)
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

## Relationship to other storage tiers

```
WRITE PATH (fastest to slowest):

  Tier 0: RAM write cache         ~0.001ms  (DashMap insert)
    |
    v (background, ~100ms)
  Tier 1: Log-structured store    ~0.002ms  (segment append, <=64KB only)
       OR
  Tier 1: Pre-opened temp pool    ~0.002ms  (write to pre-opened slot, all sizes)
    |
    v (rename worker drains)
  Tier 2: File tier               ~0.4ms    (mkdir + file create)

READ PATH (checked in order):

  Tier 0: RAM write cache         ~0.001ms  (DashMap lookup)
  Tier 1: Log store index         ~0.05ms   (HashMap + mmap slice)
       OR
  Tier 1: pending_renames table   ~0.001ms  (DashMap lookup) + ~0.1ms file read
  Tier 2: File tier               ~0.7ms    (file open + read)
```

Each tier is a fallback for the one above. Writes flow down asynchronously.
Reads check each tier in order, returning on the first hit.

The log store and the temp pool are alternatives at tier 1, gated by
`--write-tier`. See [write-pool.md](write-pool.md) for the pool design
and the benchmark plan.
