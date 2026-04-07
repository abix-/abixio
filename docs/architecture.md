# Architecture

AbixIO design principles, project structure, single-node storage model, and
cluster-control direction.

## Design principles

1. **Data is erasure-coded, metadata is replicated.** Every backend gets identical
   `meta.json` (only `erasure.index` and `checksum` vary per shard). Data shards
   are Reed-Solomon encoded.

2. **Pluggable storage backends.** The `Backend` trait defines the per-volume storage
   interface. `LocalVolume` implements it for local directories. `RemoteVolume`
   implements it over HTTP for volumes on other nodes. The volume pool treats all
   backends identically -- it does not know whether a volume is local or remote.
   The `ShardWriter` trait provides streaming writes: `LocalShardWriter` writes
   directly to files, `RemoteShardWriter` buffers per-shard then POSTs on finalize.

3. **Deterministic shard placement.** Each object's key is mapped
   deterministically onto shard locations. The placement planner assigns shards
   to volumes, and `volume_ids` is stored in `meta.json` so decode and heal can
   identify which volume holds each shard.

4. **Quorum rules.** Write quorum = data_n+1 (or data_n when parity==0).
   Read quorum = data_n. Delete quorum = data_n+1 (or data_n when parity==0).

5. **Atomic writes.** Write to `.abixio.tmp/<uuid>/`, rename to final location
   (for local backends; other backends handle atomicity in their own way).

6. **Bitrot detection.** Per-shard blake3 checksum in metadata (SIMD-accelerated).
   Bad checksums treated as missing shards, reconstructed via Reed-Solomon.

7. **Single meta.json per object.** All version metadata in one file, matching
   MinIO's `xl.meta` pattern. See [storage-layout.md](storage-layout.md).

8. **Per-object fault tolerance.** Each object stores its FTT in `meta.json`.
   The encode path resolves FTT per-request (per-object > bucket default).
   The decode and heal paths read FTT from metadata. Objects with different
   FTT coexist in the same bucket and pool. See [per-object-ec.md](per-object-ec.md).

9. **Volume pool model.** Volumes form a pool across all nodes. Objects use all
   volumes by default. FTT determines the data/parity split, spreading I/O
   across nodes.

10. **Self-describing volumes.** Every volume carries `.abixio.sys/volume.json`
    with its identity (deployment, set, node, volume UUIDs) and the full pool
    membership. A fresh binary pointed at formatted volumes can reconstruct
    the cluster without external config. See [storage-layout.md](storage-layout.md).

11. **Internode shard RPC.** Remote volumes are accessed over HTTP via
    `/_storage/v1/*` endpoints. Each request carries a JWT signed with the S3
    credentials. The storage server dispatches to the local `LocalVolume` for
    the target volume path.

12. **Cluster control fences unsafe nodes.** Multi-node control-plane behavior
    must fail closed. A node that cannot confirm safe cluster state stops
    serving instead of risking stale writes or split-brain.

## Data flow

### PUT path (streaming)

```
HTTP request body (chunked)
  -> s3s parses headers, extracts body as Stream<Bytes>
  -> AbixioS3::put_object() checks versioning config (cached in memory)
  -> VolumePool::put_object_stream() opens ShardWriters in parallel
  -> encode_and_write() reads body in 1MB blocks:
       for each block:
         MD5 hasher updates (inline, for ETag)
         split_data() divides block into data_n shards
         RS encode adds parity_n shards
         blake3 hasher updates per shard (inline, for integrity)
         write_chunk() to each ShardWriter (sequential -- see layer-optimization.md)
  -> finalize() writes meta.json on each disk
  -> return ETag to client
```

### GET path (mmap)

```
HTTP GET request
  -> s3s dispatches to AbixioS3::get_object()
  -> non-range, non-versioned: streaming path
       VolumePool::get_object_stream():
         1+0 (no EC) fast path:
           mmap shard file via Backend::mmap_shard()
           Bytes::from_owner(mmap) -> yield 4MB .slice() chunks
           zero-copy: no RS decode, no allocation, no spawned task
         EC (N+M) path:
           mmap all shard files in parallel via Backend::mmap_shard()
           spawns background task:
             for each 4MB decode block:
               slice directly into mmap regions (zero-copy read)
               RS decode the block
               send decoded Bytes via mpsc channel
       -> stream wrapped in SyncStream -> StreamingBlob -> hyper response body
  -> range/versioned: buffered path (read full object, slice, respond)
```

1+0 fast path serves files at 1220 MB/s (1GB via curl). EC path decodes
at 675-803 MB/s (4-disk). See [layer-optimization.md](layer-optimization.md)
for performance numbers and optimization history.

## Cluster

See [cluster.md](cluster.md).

## Comparison

See [comparison.md](comparison.md).

## Project structure

```
src/
  main.rs                 # CLI entry, parse args, start server
  lib.rs                  # module re-exports
  cluster/
    mod.rs                # persisted cluster state, node monitoring, fencing, cluster types
    identity.rs           # node identity exchange and boot sequence
    placement.rs          # deterministic node-first placement planner and invariants
  config.rs               # Config struct (clap derive) + {N...M} range expansion
  query.rs                # URL query string parsing
  storage/
    mod.rs                # Backend trait, ShardWriter trait, Store trait, StorageError
    metadata.rs           # ObjectMetaFile, ObjectMeta, ErasureMeta, PutOptions
    bitrot.rs             # sha256_hex(), md5_hex(), blake3_hex()
    local_volume.rs       # LocalVolume: Backend + LocalShardWriter (streaming file I/O)
    remote_volume.rs      # RemoteVolume: Backend + RemoteShardWriter (buffer + HTTP POST)
    storage_server.rs     # Storage REST server: dispatches /_storage/v1/* to local volumes
    internode_auth.rs     # JWT sign/validate for internode RPC
    volume_pool.rs        # VolumePool: volume pool with per-object FTT resolution
    erasure_encode.rs     # unified streaming encode: encode_and_write via ShardWriter trait
    erasure_decode.rs     # read + decode: buffered (read_and_decode) and streaming (read_and_decode_stream)
    volume.rs             # VolumeFormat: read/write .abixio.sys/volume.json
  s3_service.rs           # impl S3 for AbixioS3: streaming GET, versioning cache, thin adapter (s3s)
  s3_auth.rs              # impl S3Auth: SigV4 credential lookup (s3s)
  s3_access.rs            # impl S3Access: cluster fencing check (s3s)
  s3_route.rs             # AbixioDispatch: admin + storage RPC bypass, s3s passthrough
  admin/
    mod.rs                # HealStats shared state (atomic counters, uptime)
    handlers.rs           # Admin API handlers (status, disks, healing, inspect, bucket EC)
    types.rs              # Admin JSON response structs (serde Serialize)
  multipart/
    mod.rs                # multipart upload state, part encode/decode, assembly
  heal/
    mod.rs
    mrf.rs                # MRF queue (bounded channel, dedup)
    scanner.rs            # Per-object scan cooldown tracking
    worker.rs             # heal_object(), MRF drain worker, scanner loop (per-object EC aware)
tests/
  support/                # distributed test harness and controlled backends
  s3_integration.rs       # S3 API integration tests
  admin_integration.rs    # admin API integration tests
  distributed_placement_integration.rs # 4-node placement and fencing tests
  e2e.py                  # end-to-end Python test (starts server, exercises S3 + admin)
docs/
  architecture.md         # this file
  cluster.md              # cluster control design, node exchange, fencing
  storage-layout.md       # volume identity, metadata layers, directory structure
  per-object-ec.md        # per-object erasure coding, bucket EC config, volume pools
  admin-api.md            # admin API endpoints (status, volumes, heal, inspect, bucket EC)
  versioning.md           # S3 object versioning
  tagging.md              # object and bucket tagging
  presigned-urls.md       # presigned URL authentication
  conditional-requests.md # If-Match, If-None-Match, etc.
  error-responses.md      # error XML format, codes, request ID
  healing.md              # erasure healing, MRF, scanner
  encryption.md           # encryption design (pending)
  multipart-upload.md     # multipart upload lifecycle
  bucket-policy.md        # bucket policy storage (enforcement pending)
  s3-compliance.md        # S3 API compliance audit
```

## Dependencies

| Crate | Purpose |
|---|---|
| `s3s` | S3 protocol layer: SigV4 auth, XML serialization, routing, DTOs |
| `reed-solomon-erasure` | erasure coding (parity >= 1; 0-parity bypasses) |
| `serde` / `serde_json` | metadata serialization |
| `tokio` | async runtime |
| `hyper` / `hyper-util` | HTTP server |
| `sha2` / `md-5` / `hmac` / `hex` | checksums + internode auth |
| `clap` | CLI args |
| `tracing` | structured logging |
| `uuid` | version IDs, request IDs |
| `jsonwebtoken` | JWT sign/validate (internode RPC auth) |
| `reqwest` | HTTP client (internode RPC) |
| `async-trait` / `futures` | async trait support |
