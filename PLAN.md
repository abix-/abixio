# AbixIO

S3-compatible erasure-coded object store. Rust. Single binary.

## Problem

Hard drives die. All of them, eventually. Your data should not die with them.

MinIO solved this elegantly: spread data across disks using erasure coding, expose
it via S3. But MinIO targets enterprise. It requires a minimum of 4 disks, assumes
uniform hardware, and optimizes for throughput at scale.

AbixIO takes the same core idea but targets home and personal use:

- **1 disk minimum.** Start with a single folder, get S3-compatible storage. Add
  disks and parity later as your setup grows.
- **Fully configurable data/parity split.** Each disk is either data or parity,
  entirely up to the user. `--data 1 --parity 0` is valid. So is `--data 3 --parity 5`.
- **A disk is just a storage backend.** Could be a local directory, an NFS mount,
  a USB drive, or a cloud storage service. AbixIO defines a `Backend` trait --
  any type that implements it can serve as an erasure "disk".
- **Assume every disk is fallible.** Any disk can fail at any time. Parity shards
  let you survive failures up to the parity count.

Not a goal: highest performance, distributed clustering, or enterprise features.
The goal is protecting personal data against disk failures with minimal setup.

## Configuration

```
--disks /mnt/d1,/mnt/d2,/mnt/d3    # directory paths (1 or more)
--data 2                             # data shards
--parity 1                           # parity shards (0 or more)
--listen :9000                       # HTTP listen address
--no-auth                            # disable S3 auth
--scan-interval 10m                  # integrity scanner loop interval
--heal-interval 24h                  # per-object recheck cooldown
--mrf-workers 2                      # MRF heal worker count
```

Rules:
- `len(disks) == data + parity`
- `data >= 1`
- `parity >= 0` (zero = no redundancy)

| Config | Behavior |
|---|---|
| 1 disk, 1 data, 0 parity | Plain S3 storage. No redundancy. |
| 2 disks, 1 data, 1 parity | Mirror-like. Survives 1 disk failure. |
| 4 disks, 2 data, 2 parity | Classic erasure coding. Survives 2 failures. |
| 6 disks, 2 data, 4 parity | Heavy parity. Survives 4 failures. 33% capacity. |

## Architecture

Uses standard erasure coding patterns common to distributed storage systems.
Original Rust implementation.

### Design principles

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

### Comparison with MinIO

| Aspect | MinIO | AbixIO |
|--------|-------|--------|
| Language | Go | Rust |
| Scope | distributed multi-node | single process, local dirs |
| Min disks | 4 (enforced) | 1 (with 0 parity) |
| Metadata | binary msgpack `xl.meta` with versioning | plain JSON `meta.json`, no versioning |
| Disk layout | `<disk>/<vol>/<obj>/xl.meta` + `<datadir>/part.1` | `<disk>/<bucket>/<key>/shard.dat` + `meta.json` |
| Small objects | inlined into xl.meta | always separate shard.dat |
| Auth | full IAM, STS, LDAP, OpenID | single access key/secret or no-auth |
| Healing | crawler + bloom + admin API | MRF queue + periodic scanner |

### Metadata on disk

```
For object "photos/cat.jpg" with distribution [2, 0, 3, 1]:
  disk 0 gets shard 1    disk 1 gets shard 3
  disk 2 gets shard 0    disk 3 gets shard 2

Each disk's meta.json:
  {size, etag, content_type, created_at,
   erasure: {data:2, parity:2, index:<shard_idx>, distribution:[2,0,3,1]},
   checksum: <sha256 of THIS disk's shard>}
```

### Healing

Two paths work together to keep all shards healthy. Adapted from MinIO's
three-path architecture (MRF + background workers + global scanner), simplified
for single-process local storage.

#### Path 1: MRF (Most Recently Failed), Reactive

When `put_object` succeeds with quorum but fails on some disks, the partial
failure is immediately enqueued to the MRF queue (`heal/mrf.rs`). A background
tokio task drains the queue and calls `heal_object()` for each entry.

- Bounded channel (1000 entries), deduplicated by (bucket, key)
- On overflow: silently dropped (scanner catches it later)
- Single drain worker reads entries and heals them
- After healing, `mark_done()` removes from dedup set

#### Path 2: Integrity Scanner, Proactive

Background tokio task runs on `--scan-interval` (default 10m). Walks all
buckets and objects on disk 0, checks shard integrity across ALL disks.

- Per-object cooldown via `ScanState` (`heal/scanner.rs`): skips objects
  checked within `--heal-interval` (default 24h)
- For each object: reads meta from all disks, verifies `shard.dat` checksum
  against `meta.json` checksum on every disk
- If any shard is missing or corrupt, enqueues to MRF for repair

#### heal_object(), the core function

`heal/worker.rs:heal_object()` does the actual repair:

```
1. Read meta.json + shard.dat from ALL disks
2. Find consensus metadata:
   - Group by quorum_eq() (same size, etag, content_type, created_at,
     erasure config, distribution, ignoring per-shard index/checksum)
   - Pick the group with the most members (must >= data_n)
3. Classify each shard position via the distribution map:
   - Good: meta matches consensus AND shard checksum matches
   - NeedsRepair: meta missing, doesn't match, or checksum mismatch
4. If good_count < data_n: return Unrecoverable (can't heal)
5. Reed-Solomon reconstruct missing shards from good ones
6. Write repaired shards atomically (tmp dir + rename)
```

#### Design decisions

| Decision | Rationale |
|---|---|
| Heal to tmp, then rename | Atomicity. Partial heal won't corrupt state |
| Bounded MRF channel, drop on overflow | Best-effort; scanner catches anything missed |
| Single MRF receiver/worker | Simple, sufficient for single-process use |
| Per-object scan cooldown | Don't re-check recently verified objects |
| No namespace locking during heal | Single process; concurrent write would just re-heal |
| No MRF persistence to disk | Scanner catches anything lost on restart |
| Consensus by quorum_eq majority | Detects stale/outdated metadata without extra state |

#### Graceful shutdown

Ctrl-C triggers `tokio::sync::watch` channel. Scanner and MRF worker check
the channel between iterations and exit cleanly. HTTP server stops accepting
new connections via `tokio::select!`.

## Project structure

```
src/
  main.rs                 # CLI entry, parse args, start server
  lib.rs                  # module re-exports
  config.rs               # Config struct (clap derive) + validation
  storage/
    mod.rs                # Backend trait, Store trait, StorageError, BackendInfo
    metadata.rs           # ObjectMeta, ObjectInfo, BucketInfo, ListResult
    bitrot.rs             # sha256_hex(), md5_hex()
    disk.rs               # LocalDisk: Backend impl for local directories
    erasure_set.rs        # ErasureSet: Store impl over Vec<Box<dyn Backend>>
    erasure_encode.rs     # split_data + reed-solomon encode + write to backends
    erasure_decode.rs     # read from backends + bitrot check + reconstruct
  s3/
    mod.rs
    router.rs             # HTTP server + request dispatch
    handlers.rs           # S3 operation handlers
    auth.rs               # AWS Sig V4 verification
    response.rs           # S3 XML response structs (quick-xml)
    errors.rs             # S3 error codes + XML + error mapping
  heal/
    mod.rs
    mrf.rs                # MRF queue (bounded channel, dedup)
    scanner.rs            # Per-object scan cooldown tracking
    worker.rs             # heal_object(), MRF drain worker, scanner loop
```

## Dependencies

```toml
reed-solomon-erasure = "6"    # erasure coding (parity >= 1 required; 0-parity bypasses)
serde / serde_json            # metadata serialization
tokio                         # async runtime
hyper / hyper-util            # HTTP server
quick-xml                     # S3 XML responses
sha2 / md-5 / hmac / hex     # checksums + auth
clap                          # CLI args
tracing                       # logging
thiserror / anyhow            # errors
subtle                        # constant-time compare (auth)
```

Note: `reed-solomon-erasure` requires `parity >= 1`. The 0-parity path bypasses
Reed-Solomon entirely. Data is split into shards without encoding.

## Current status

### Done (117 tests passing, 0 clippy warnings)

| Component | File | Status | Tests |
|-----------|------|--------|-------|
| Config | `config.rs` | DONE | 10 |
| Backend trait | `storage/mod.rs` | DONE | (used by all storage tests) |
| Store trait | `storage/mod.rs` | DONE | (used by ErasureSet tests) |
| Metadata | `storage/metadata.rs` | DONE | 5 |
| Bitrot | `storage/bitrot.rs` | DONE | 4 |
| LocalDisk backend | `storage/disk.rs` | DONE | 13 |
| Erasure encode | `storage/erasure_encode.rs` | DONE | 8 |
| Erasure decode | `storage/erasure_decode.rs` | DONE | (tested via erasure_set) |
| ErasureSet | `storage/erasure_set.rs` | DONE | 17 |
| S3 auth (Sig V4) | `s3/auth.rs` | DONE | 4 |
| S3 errors | `s3/errors.rs` | DONE | 5 |
| S3 responses | `s3/response.rs` | DONE | 3 |
| MRF queue | `heal/mrf.rs` | DONE | 4 |
| Scanner state | `heal/scanner.rs` | DONE | 4 |
| Heal worker | `heal/worker.rs` | DONE | 8 |
| HTTP router | `s3/router.rs` | DONE | (tested via integration) |
| HTTP handlers | `s3/handlers.rs` | DONE | (tested via integration) |
| main.rs wiring | `main.rs` | DONE | (tested via integration) |
| S3 integration | `tests/s3_integration.rs` | DONE | 14 |
| Admin integration | `tests/admin_integration.rs` | DONE | 13 |

### Remaining

| Component | File | Status | What's needed |
|-----------|------|--------|---------------|
| Heal workers | `heal/worker.rs` | DONE | drain MRF queue, run scanner loop, repair shards |
| MRF auto-enqueue | `storage/erasure_encode.rs` | DONE | enqueue on partial write failure |
| Cloud backends | `storage/` | NOT STARTED | Google Drive, OneDrive, etc. Backend trait is ready. |
| Replication | | NOT STARTED | async site-to-site (post-MVP) |
| Graceful shutdown | `main.rs` | DONE | ctrl-c handler, drain workers |
| Multipart upload | | NOT STARTED | post-MVP |

### Progress: ~95%

The server is fully functional with background healing and pluggable storage
backends. Verified with live smoke test on 2026-04-04:
- Create bucket, put/get/head/delete objects, all working
- Erasure resilience confirmed: deleted 2 of 4 disk shards, data recovered
- S3 XML responses (ListBuckets, ListObjects) working
- Background healing: MRF auto-enqueue on partial writes, scanner loop,
  heal_object() reconstructs missing/corrupt shards
- Backend trait extracted: storage layer is generic over any backend type
- Graceful shutdown via ctrl-c
- 117 tests (90 unit + 27 integration), 0 clippy warnings

What remains is cloud storage backends and post-MVP features (replication,
multipart upload, versioning).

## Build & test

```bash
cargo build --release    # 2.1 MB binary
cargo test               # 93 tests
cargo clippy -- -D warnings

# run server
mkdir -p C:/tmp/abixio/{d1,d2,d3,d4}
./target/release/abixio --listen 0.0.0.0:9000 \
  --disks C:/tmp/abixio/d1,C:/tmp/abixio/d2,C:/tmp/abixio/d3,C:/tmp/abixio/d4 \
  --data 2 --parity 2 --no-auth

# S3 operations via curl (or aws cli with --no-sign-request)
curl -X PUT http://localhost:9000/test                                           # create bucket
curl -X PUT -d "hello" http://localhost:9000/test/hello.txt                      # put object
curl http://localhost:9000/test/hello.txt                                        # get object
curl -I http://localhost:9000/test/hello.txt                                     # head object
curl http://localhost:9000/                                                      # list buckets
curl "http://localhost:9000/test?list-type=2"                                    # list objects
curl -X DELETE http://localhost:9000/test/hello.txt                              # delete object
echo "hello" | aws s3 cp - s3://test/hello.txt --endpoint-url http://localhost:9000 --no-sign-request
aws s3 cp s3://test/hello.txt - --endpoint-url http://localhost:9000 --no-sign-request

# erasure resilience: delete 2 of 4 disk shards, GET still works
rm -rf /tmp/abixio/d3/test/hello.txt /tmp/abixio/d4/test/hello.txt
aws s3 cp s3://test/hello.txt - --endpoint-url http://localhost:9000 --no-sign-request
```

## Notes

- MVP reads entire object into memory (no streaming). Fine for objects < 1GB.
- Keys with `/` create nested directories, matching S3 behavior.
- No multipart upload in MVP.
- No object versioning.
- A "disk" is any type implementing the `Backend` trait. Currently only
  `LocalDisk` (local directory) exists. Cloud backends (Google Drive, OneDrive,
  etc.) can be added by implementing `Backend` without changing core logic.
  User is responsible for placing backends on separate media/services if they
  want actual fault tolerance.
- Encryption decision deferred. See `docs/encryption.md`. Leaning toward
  external (FDE + client-side age encryption, no AbixIO code changes).
