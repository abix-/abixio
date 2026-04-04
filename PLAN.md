# AbixIO

S3-compatible erasure-coded object store. Rust. Single binary.

## Problem

Hard drives die. All of them, eventually. Your data should not die with them.

MinIO solved this elegantly: spread data across disks using erasure coding, expose
it via S3. But MinIO targets enterprise -- it requires a minimum of 4 disks, assumes
uniform hardware, and optimizes for throughput at scale.

AbixIO takes the same core idea but targets home and personal use:

- **1 disk minimum.** Start with a single folder, get S3-compatible storage. Add
  disks and parity later as your setup grows.
- **Fully configurable data/parity split.** Each disk is either data or parity,
  entirely up to the user. `--data 1 --parity 0` is valid. So is `--data 3 --parity 5`.
- **A disk is just a path.** Could be a mount point, a folder on the same volume,
  an NFS share, a USB drive. AbixIO does not care -- it writes to directories.
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

1. **Data is erasure-coded, metadata is replicated.** Every disk gets identical
   `meta.json` (only `erasure.index` and `checksum` vary per disk). Data shards
   are Reed-Solomon encoded.

2. **Hash-based shard distribution.** Each object's key is hashed to produce a
   permutation of disk indices. Different objects spread I/O across disks.
   Distribution stored in `meta.json` for reconstruction.

3. **Quorum rules.** Write quorum = data_n+1 (or data_n when parity==0).
   Read quorum = data_n. Delete quorum = data_n+1 (or data_n when parity==0).

4. **Atomic writes.** Write to `.abixio.tmp/<uuid>/`, rename to final location.

5. **Bitrot detection.** Per-shard SHA256 checksum in metadata. Bad checksums
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

**MRF queue** -- reactive. When put_object succeeds with quorum but fails on some
disks, immediately enqueue (bucket, key) for repair. In-memory bounded channel,
deduplicated.

**Integrity scanner** -- proactive. Background loop walks all objects. Per-object
cooldown: skips objects checked within `heal_interval`. Verifies shard presence +
checksum on every disk. Reconstructs/repairs when quorum available.

## Project structure

```
src/
  main.rs                 # CLI entry, parse args, start server
  lib.rs                  # module re-exports
  config.rs               # Config struct (clap derive) + validation
  storage/
    mod.rs                # Store trait, StorageError enum
    metadata.rs           # ObjectMeta, ObjectInfo, BucketInfo, ListResult
    bitrot.rs             # sha256_hex(), md5_hex()
    disk.rs               # DiskStorage: single-disk I/O, atomic writes
    erasure_set.rs        # ErasureSet: Store impl, quorum logic
    erasure_encode.rs     # split_data + reed-solomon encode + parallel write
    erasure_decode.rs     # parallel read + bitrot check + reconstruct
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
Reed-Solomon entirely -- data is split into shards without encoding.

## Current status

### Done (93 tests passing, 0 clippy warnings, 2.1 MB release binary)

| Component | File | Status | Tests |
|-----------|------|--------|-------|
| Config | `config.rs` | DONE | 10 |
| Store trait | `storage/mod.rs` | DONE | -- |
| Metadata | `storage/metadata.rs` | DONE | 5 |
| Bitrot | `storage/bitrot.rs` | DONE | 4 |
| Disk I/O | `storage/disk.rs` | DONE | 12 |
| Erasure encode | `storage/erasure_encode.rs` | DONE | 8 |
| Erasure decode | `storage/erasure_decode.rs` | DONE | -- (tested via erasure_set) |
| ErasureSet | `storage/erasure_set.rs` | DONE | 17 |
| S3 auth (Sig V4) | `s3/auth.rs` | DONE | 4 |
| S3 errors | `s3/errors.rs` | DONE | 5 |
| S3 responses | `s3/response.rs` | DONE | 3 |
| MRF queue | `heal/mrf.rs` | DONE | 4 |
| Scanner state | `heal/scanner.rs` | DONE | 4 |
| HTTP router | `s3/router.rs` | DONE | -- (tested via integration) |
| HTTP handlers | `s3/handlers.rs` | DONE | -- (tested via integration) |
| main.rs wiring | `main.rs` | DONE | -- |
| Integration tests | `tests/s3_integration.rs` | DONE | 13 |

### Remaining

| Component | File | Status | What's needed |
|-----------|------|--------|---------------|
| Heal workers | `heal/` | PRIMITIVES ONLY | drain MRF queue, run scanner loop, repair shards |
| MRF auto-enqueue | `storage/erasure_encode.rs` | NOT WIRED | enqueue on partial write failure |
| Replication | -- | NOT STARTED | async site-to-site (post-MVP) |
| Graceful shutdown | `main.rs` | NOT STARTED | ctrl-c handler, drain in-flight |
| Multipart upload | -- | NOT STARTED | post-MVP |

### Progress: ~85%

The server is fully functional. Verified with live smoke test on 2026-04-04:
- Create bucket, put/get/head/delete objects -- all working
- Erasure resilience confirmed: deleted 2 of 4 disk shards, data recovered
- S3 XML responses (ListBuckets, ListObjects) working
- 93 tests (80 unit + 13 integration), 0 clippy warnings

What remains is background healing (the MRF queue and scanner primitives exist
but aren't wired into background tasks yet) and post-MVP features (replication,
multipart upload, graceful shutdown).

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
- Keys with `/` create nested directories -- matches S3 behavior.
- No multipart upload in MVP.
- No object versioning.
- A "disk" is just a directory path. User is responsible for placing disks on
  separate physical media if they want actual fault tolerance.
- Encryption decision deferred -- see `docs/encryption.md`. Leaning toward
  external (FDE + client-side age encryption, no AbixIO code changes).
