# AbixIO

S3-compatible object store with erasure coding. Single Rust binary.

## Problem

Hard drives die. All of them, eventually. Your data should not die with them.

AbixIO spreads your data across disks using erasure coding and exposes it via the S3 API. Lose a disk, lose nothing. Any S3 client works out of the box.

## How it works

- Data is split into shards using Reed-Solomon erasure coding
- Shards are distributed across disks with hash-based permutation
- Metadata is replicated to every disk (not erasure-coded)
- Per-shard SHA256 checksums detect bitrot automatically
- Failed/corrupt shards are reconstructed from remaining good shards

## Configuration

```
abixio --listen 0.0.0.0:9000 \
  --disks /mnt/d1,/mnt/d2,/mnt/d3,/mnt/d4 \
  --data 2 --parity 2 --no-auth
```

| Config | Behavior |
|---|---|
| 1 disk, 1 data, 0 parity | Plain S3 storage. No redundancy. |
| 2 disks, 1 data, 1 parity | Mirror-like. Survives 1 disk failure. |
| 4 disks, 2 data, 2 parity | Erasure coding. Survives 2 failures. |
| 6 disks, 2 data, 4 parity | Heavy parity. Survives 4 failures. |

Rules:
- `data >= 1`, `parity >= 0`
- Number of disks must equal `data + parity`
- A disk is just a directory path -- same volume, different volume, NFS, USB, whatever

## Usage

```bash
# create bucket
curl -X PUT http://localhost:9000/mybucket

# upload
curl -X PUT -d "hello world" http://localhost:9000/mybucket/hello.txt

# download
curl http://localhost:9000/mybucket/hello.txt

# list buckets
curl http://localhost:9000/

# list objects
curl "http://localhost:9000/mybucket?list-type=2"

# delete
curl -X DELETE http://localhost:9000/mybucket/hello.txt
```

Works with any S3 client: AWS CLI, rclone, s3cmd, boto3, etc.

## Build

```bash
cargo build --release
# produces target/release/abixio (~2 MB)
```

## Status: alpha (actively developed)

The server is functional and tested but not production-ready.

**Working now:**
- S3 API: PUT/GET/HEAD/DELETE objects, create/list buckets, list objects with prefix/delimiter
- Erasure coding across 1-N disks with configurable data/parity shards
- Bitrot detection via per-shard SHA256 checksums
- AWS Signature V4 authentication (or --no-auth for local use)
- Hash-based shard distribution across disks
- 93 tests (80 unit + 13 integration), 0 clippy warnings
- 2.1 MB release binary

**Not yet implemented:**
- Background healing (MRF queue + integrity scanner primitives exist, not wired to workers)
- Multipart upload
- Object versioning
- Bucket replication
- Graceful shutdown

See [PLAN.md](PLAN.md) for full architecture and progress (~85% complete).

## License

[GNU General Public License v3.0](LICENSE)
