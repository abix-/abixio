# Architecture

AbixIO design principles, project structure, and comparison with MinIO.

## Design principles

1. **Data is erasure-coded, metadata is replicated.** Every backend gets identical
   `meta.json` (only `erasure.index` and `checksum` vary per shard). Data shards
   are Reed-Solomon encoded.

2. **Pluggable storage backends.** The `Backend` trait defines the per-disk storage
   interface. `LocalDisk` implements it for local directories. Any storage service
   that can read/write blobs by key can implement `Backend` and plug into the
   erasure set without changing the core logic.

3. **Hash-based shard distribution.** Each object's key is hashed to produce a
   permutation of backend indices. Different objects spread I/O across backends.
   Distribution stored in `meta.json` for reconstruction.

4. **Quorum rules.** Write quorum = data_n+1 (or data_n when parity==0).
   Read quorum = data_n. Delete quorum = data_n+1 (or data_n when parity==0).

5. **Atomic writes.** Write to `.abixio.tmp/<uuid>/`, rename to final location
   (for local backends; other backends handle atomicity in their own way).

6. **Bitrot detection.** Per-shard SHA256 checksum in metadata. Bad checksums
   treated as missing shards, reconstructed via Reed-Solomon.

7. **Single meta.json per object.** All version metadata in one file, matching
   MinIO's `xl.meta` pattern. See [storage-layout.md](storage-layout.md).

## Comparison with MinIO

| Aspect | MinIO | AbixIO |
|---|---|---|
| Language | Go | Rust |
| Scope | distributed multi-node | single process, disk pool |
| Erasure coding | cluster-level EC ratio | per-object EC (data/parity per object) |
| EC config | fixed per server pool | per-object header > bucket config > server default |
| Min disks | 4 (enforced) | 1 (with 0 parity) |
| Metadata | binary msgpack `xl.meta` with versioning | JSON `meta.json` with versions array |
| Disk layout | `<disk>/<vol>/<obj>/xl.meta` + `<datadir>/part.1` | `<disk>/<bucket>/<key>/meta.json` + `shard.dat` |
| Small objects | inlined into xl.meta | always separate shard.dat |
| Auth | full IAM, STS, LDAP, OpenID | single access key/secret or no-auth + presigned URLs |
| Healing | crawler + bloom + admin API | MRF queue + periodic scanner |
| Versioning | full S3 versioning | full S3 versioning (enable/suspend per bucket) |
| Tagging | object + bucket tags | object + bucket tags |

## Project structure

```
src/
  main.rs                 # CLI entry, parse args, start server
  lib.rs                  # module re-exports
  config.rs               # Config struct (clap derive) + validation
  query.rs                # URL query string parsing
  storage/
    mod.rs                # Backend trait, Store trait, StorageError, BackendInfo
    metadata.rs           # ObjectMetaFile, ObjectMeta, ErasureMeta, ObjectInfo, VersioningConfig
    bitrot.rs             # sha256_hex(), md5_hex()
    disk.rs               # LocalDisk: Backend impl for local directories
    erasure_set.rs        # ErasureSet: Store impl over Vec<Box<dyn Backend>>
    erasure_encode.rs     # split_data + reed-solomon encode + write to backends
    erasure_decode.rs     # read from backends + bitrot check + reconstruct
  s3/
    mod.rs
    handlers.rs           # S3 handler dispatch + all endpoint implementations
    auth.rs               # AWS Sig V4 verification + presigned URL validation
    response.rs           # S3 XML response structs (quick-xml)
    errors.rs             # S3 error codes + XML + error mapping
  admin/
    handlers.rs           # Admin API handlers (status, disks, healing, inspect)
  multipart/
    mod.rs                # multipart upload state, part encode/decode, assembly
  heal/
    mod.rs
    mrf.rs                # MRF queue (bounded channel, dedup)
    scanner.rs            # Per-object scan cooldown tracking
    worker.rs             # heal_object(), MRF drain worker, scanner loop
tests/
  s3_integration.rs       # 47 S3 API integration tests
  admin_integration.rs    # 13 admin API integration tests
docs/
  architecture.md         # this file
  storage-layout.md       # disk layout, meta.json format
  versioning.md           # S3 object versioning
  tagging.md              # object and bucket tagging
  presigned-urls.md       # presigned URL authentication
  conditional-requests.md # If-Match, If-None-Match, etc.
  error-responses.md      # error XML format, codes, request ID
  healing.md              # erasure healing, MRF, scanner
  encryption.md           # encryption design (pending)
  multipart-upload.md     # multipart upload lifecycle
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
| `form_urlencoded` | URL query parsing |
