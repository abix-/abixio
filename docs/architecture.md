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
   backends identically; it does not know whether a volume is local or remote.
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

13. **Log-structured storage for small objects.** Objects <= 64KB are written
    as needles to append-only log segments. One sequential append per disk
    instead of mkdir + shard.dat + meta.json (3 fs ops). The log IS the
    permanent storage. No flush, no second format.
    In-memory index maps bucket+key to segment:offset. GC reclaims dead space.
    Large objects keep the file-per-object layout. Recent published 4KB
    numbers in the docs are ~0.91ms PUT and ~0.76ms GET in the dedicated
    log-store benchmark. No fsync. Page cache
    serves both read and write paths. See [write-log.md](write-log.md).

14. **Pre-opened temp file pool (alternative to the log store).**
    A write path that keeps a small pool of already-open temp files
    per disk under `.abixio.sys/tmp/`. PUT writes shard bytes and meta
    JSON into a slot's two pre-opened files and acks. The mkdir +
    rename to the destination happen on a background worker. Two
    syscalls on the hot path instead of seven. Designed as a
    replacement for the log store, motivated by GC simplicity (one
    file per object means `unlink()` reclaims space natively, no
    compactor needed). Current benchmark conclusion is that the pool
    wins a mid-range write window, while the log store wins <=64KB and
    the file tier wins large writes. See [write-pool.md](write-pool.md).

## Data flow

### PUT path

AbixIO now has multiple write branches with different ack semantics.
The authoritative end-to-end trace lives in [write-path.md](write-path.md).
This section stays as a summary only.

```
HTTP request body
  -> s3s parses headers, checks versioning config (cached)
  -> VolumePool resolves EC, placement, and the write branch
  -> possible branches:
       RAM write cache (ack from RAM, disk later on explicit flush)
       log store (append is the final resting place)
       write pool (ack from temp files, rename later)
       file tier (ack after final files are written)
       remote volume RPC (target node executes its own local branch)
  -> ack
```

For the exact branch matrix, per-layer responsibilities, and measured
timings, see [write-path.md](write-path.md).

### GET path

```
HTTP GET request
  -> s3s dispatches to AbixioS3::get_object()

  check in-memory log index first:
    FOUND -> mmap slice from log segment (zero-copy)
    NOT FOUND -> fall through to file tier:

  file tier (non-range, non-versioned):
    1+0 (no EC):
      mmap shard file, Bytes::from_owner(mmap), yield entire file as single Bytes
      zero-copy: no RS decode, no allocation, no spawned task
    EC (N+M):
      mmap all shard files, yield shard slices directly as Bytes frames
      zero-copy when all shards healthy, RS reconstruct only when degraded

  range/versioned: buffered path (read full object, slice, respond)
```

The 1+0 fast path uses zero-copy mmap. EC uses zero-alloc mmap slices.
Small-object reads come from the log segment mmap. End-to-end PUT and
GET numbers per tier and per size live in
[write-path.md::Where the time goes](write-path.md#where-the-time-goes).
The L1-L7 storage-pipeline ceilings (raw write, mmap GET, blake3, RS,
hyper, full s3s, full client with auth) live in
[layer-optimization.md](layer-optimization.md). Cross-server competitive
numbers vs MinIO and RustFS live in
[benchmarks.md](benchmarks.md#comprehensive-matrix). None of those are
duplicated here.

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
    needle.rs             # needle format for log-structured storage (serialize, checksum, msgpack)
    segment.rs            # log segment files (pre-alloc, append, seal, mmap, scan)
    log_store.rs          # log-structured store: in-memory index, segment lifecycle, recovery
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

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| `Backend`, `LocalVolume`, `RemoteVolume`, `VolumePool`, `AbixioS3`, and `AbixioDispatch` roles | Verified | `src/storage/mod.rs`, `src/storage/local_volume.rs`, `src/storage/remote_volume.rs`, `src/storage/volume_pool.rs`, `src/s3_service.rs`, `src/s3_route.rs` |
| Versioning config is cached in `s3_service.rs` | Verified | `src/s3_service.rs:74`, `96-117`, `359-366`, `552-562` |
| Small objects `<=64KB` use log-structured routing | Verified | `src/storage/volume_pool.rs:239-365`, `src/storage/local_volume.rs:564-568` |
| Log-store PUT path includes `fsync + ack` | Corrected | Code comments and implementation explicitly say no fsync: `src/storage/local_volume.rs:251-253` |
| Pool/default choice is still unsettled | Corrected | Newer docs and bench artifacts already establish the three-tier result; see `docs/benchmarks.md` and `docs/write-pool.md` |
| 1+0 mmap fast path and EC mmap decode exist | Verified | `src/s3_service.rs:458-474`, `src/storage/erasure_decode.rs`, `src/storage/volume_pool.rs` |
| Per-tier and per-size performance numbers in this doc | Removed (moved out) | Architecture doc no longer carries perf claims; see [write-path.md](write-path.md), [layer-optimization.md](layer-optimization.md), [benchmarks.md](benchmarks.md) for the canonical sources |
| Dependency table | Mostly verified at a glance | The listed crates are present and used, but I did not exhaustively reconcile every crate entry against `Cargo.toml` in this pass |

Verdict: the architecture summary is structurally accurate, but it had a few stale summary claims inherited from older benchmark/design phases. The corrected version now matches the current storage-tier story and no-fsync write model.
