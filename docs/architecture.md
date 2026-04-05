# Architecture

AbixIO design principles, project structure, single-node storage model, and
cluster-control direction.

## Design principles

1. **Data is erasure-coded, metadata is replicated.** Every backend gets identical
   `meta.json` (only `erasure.index` and `checksum` vary per shard). Data shards
   are Reed-Solomon encoded.

2. **Pluggable storage backends.** The `Backend` trait defines the per-volume storage
   interface. `LocalVolume` implements it for local directories. `RemoteVolume`
   implements it over HTTP for volumes on other nodes. The erasure set treats all
   backends identically -- it does not know whether a volume is local or remote.

3. **Deterministic shard distribution.** Each object's key is mapped
   deterministically onto shard locations. In the single-node path this is a
   backend-index permutation; in the placement-aware path it includes stable
   `epoch_id`, `set_id`, `node_ids`, and `volume_ids`. Distribution is stored in
   `meta.json` for reconstruction.

4. **Quorum rules.** Write quorum = data_n+1 (or data_n when parity==0).
   Read quorum = data_n. Delete quorum = data_n+1 (or data_n when parity==0).

5. **Atomic writes.** Write to `.abixio.tmp/<uuid>/`, rename to final location
   (for local backends; other backends handle atomicity in their own way).

6. **Bitrot detection.** Per-shard SHA256 checksum in metadata. Bad checksums
   treated as missing shards, reconstructed via Reed-Solomon.

7. **Single meta.json per object.** All version metadata in one file, matching
   MinIO's `xl.meta` pattern. See [storage-layout.md](storage-layout.md).

8. **Per-object erasure coding.** Each object owns its data/parity ratio, stored
   in `meta.json`. The encode path resolves EC per-request (object header > bucket
   config > server default). The decode and heal paths read EC from metadata.
   Objects with different EC ratios coexist in the same bucket and disk pool.
   See [per-object-ec.md](per-object-ec.md).

9. **Volume pool model.** Volumes form a pool across all nodes. Objects select a
   subset via hash-based permutation. Pool can have more volumes than any single
   object's data+parity, spreading I/O across nodes.

10. **Self-describing volumes.** Every volume carries `.abixio.sys/volume.json`
    with its identity (deployment, set, node, volume UUIDs) and the full erasure
    set membership. A fresh binary pointed at formatted volumes can reconstruct
    the cluster without external config. See [storage-layout.md](storage-layout.md).

11. **Internode shard RPC.** Remote volumes are accessed over HTTP via
    `/_storage/v1/*` endpoints. Each request carries a JWT signed with the S3
    credentials. The storage server dispatches to the local `LocalVolume` for
    the target volume path.

10. **Cluster control fences unsafe nodes.** Multi-node control-plane behavior
    must fail closed. A node that cannot confirm safe cluster state stops
    serving instead of risking stale writes or split-brain.

## Cluster Direction

AbixIO has a node-based cluster-control layer:

- node-based identity exchange at startup
- self-describing volumes with `.abixio.sys/volume.json`
- persisted cluster metadata in `.abixio.sys/cluster.json`
- cluster admin endpoints
- node monitoring and quorum tracking
- hard fencing when quorum is lost

Internode shard RPC is implemented: each node can read/write shards on remote
nodes via `RemoteVolume` over HTTP. Live topology changes, heterogeneous set
planning, and rebalance are still future work.

Nodes generate their own identity on first boot, exchange it with other nodes, and
persist the full membership on their volumes. Unsafe nodes fence themselves.

See [cluster.md](cluster.md) for the full design and current behavior.

## Competitive Positioning

The product comparison and overlap analysis now live in
[comparison.md](comparison.md).

That document covers:

- where AbixIO overlaps with MinIO, Garage, and SeaweedFS
- where the current differentiators are real
- where the roadmap is differentiated but not built yet
- a capability matrix focused on overlap, maturity, and backend flexibility

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
    mod.rs                # Backend trait, Store trait, StorageError, BackendInfo, EcConfig
    metadata.rs           # ObjectMetaFile, ObjectMeta, ErasureMeta, EcConfig, PutOptions
    bitrot.rs             # sha256_hex(), md5_hex()
    local_volume.rs       # LocalVolume: Backend impl for local directories
    remote_volume.rs      # RemoteVolume: Backend impl over HTTP (internode RPC)
    storage_server.rs     # Storage REST server: dispatches /_storage/v1/* to local volumes
    internode_auth.rs     # JWT sign/validate for internode RPC
    erasure_set.rs        # ErasureSet: volume pool with per-object EC resolution
    erasure_encode.rs     # split_data + reed-solomon encode + volume subset selection
    erasure_decode.rs     # read from backends + bitrot check + reconstruct
    volume.rs             # VolumeFormat: read/write .abixio.sys/volume.json
  s3/
    mod.rs
    router.rs             # HTTP server: TcpListener + hyper service dispatch
    handlers.rs           # S3 handler dispatch + all endpoint implementations
    auth.rs               # AWS Sig V4 verification + presigned URL validation
    response.rs           # S3 XML response structs (quick-xml)
    errors.rs             # S3 error codes + XML + error mapping
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
| `reed-solomon-erasure` | erasure coding (parity >= 1; 0-parity bypasses) |
| `serde` / `serde_json` | metadata serialization |
| `tokio` | async runtime |
| `hyper` / `hyper-util` | HTTP server |
| `quick-xml` | S3 XML request/response |
| `sha2` / `md-5` / `hmac` / `hex` | checksums + auth |
| `clap` | CLI args |
| `tracing` | structured logging |
| `uuid` | version IDs |
| `subtle` | constant-time compare (auth) |
| `jsonwebtoken` | JWT sign/validate (internode RPC auth) |
| `reqwest` | HTTP client (internode RPC) |
| `form_urlencoded` | URL query parsing |
