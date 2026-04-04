# AbixIO

**Status: alpha. Server functional with background healing, admin API, pluggable storage backends, 117 tests passing.**

S3-compatible object store with erasure coding and pluggable storage backends. Single Rust binary.

## Problem

Hard drives die. All of them, eventually. Your data should not die with them.

AbixIO spreads your data across disks using erasure coding and exposes it via the S3 API. Lose a disk, lose nothing. Any S3 client works out of the box.

## How it works

- Data is split into shards using Reed-Solomon erasure coding
- Shards are distributed across storage backends with hash-based permutation
- Each "disk" is a pluggable storage backend. Local directory today, cloud storage tomorrow
- Metadata is replicated to every backend (not erasure-coded)
- Per-shard SHA256 checksums detect bitrot automatically
- Failed/corrupt shards are automatically reconstructed from remaining good shards
- Background healing via MRF queue (reactive) and integrity scanner (proactive)

## Configuration

```
abixio --listen 0.0.0.0:10000 \
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
- A disk is a storage backend: local directory, NFS mount, USB drive, or (future) cloud storage

## Usage

```bash
# create bucket
curl -X PUT http://localhost:10000/mybucket

# upload
curl -X PUT -d "hello world" http://localhost:10000/mybucket/hello.txt

# download
curl http://localhost:10000/mybucket/hello.txt

# list buckets
curl http://localhost:10000/

# list objects
curl "http://localhost:10000/mybucket?list-type=2"

# delete
curl -X DELETE http://localhost:10000/mybucket/hello.txt
```

Works with any S3 client: AWS CLI, rclone, s3cmd, boto3, etc.

## Admin API

Management endpoints at `/_admin/*` (JSON responses). Used by [abixio-ui](https://github.com/abix-/abixio-ui) for server management. Same Sig V4 auth as S3 (or open with `--no-auth`).

```bash
# server status
curl http://localhost:10000/_admin/status

# disk health (per-disk space, status, object counts)
curl http://localhost:10000/_admin/disks

# healing status (MRF queue, scanner stats)
curl http://localhost:10000/_admin/heal

# inspect object shards (per-disk shard status)
curl "http://localhost:10000/_admin/object?bucket=mybucket&key=hello.txt"

# trigger manual heal
curl -X POST "http://localhost:10000/_admin/heal?bucket=mybucket&key=hello.txt"
```

## Build

```bash
cargo build --release
# produces target/release/abixio (~2.3 MB)
```

## What works / what doesn't

**Working (11 of ~100 S3 API operations):**
- Object CRUD: PUT, GET, HEAD, DELETE single objects
- Batch delete: POST /{bucket}?delete (DeleteObjects, up to 1000 keys/call)
- Server-side copy: PUT with x-amz-copy-source header (CopyObject)
- Bucket lifecycle: create, delete (empty only), head, list
- Object listing: ListObjectsV2 with prefix, delimiter, pagination
- Range requests: GET with Range header returns 206 Partial Content
- Custom metadata: x-amz-meta-* headers stored on PUT, returned on HEAD/GET
- Erasure coding across 1-N backends with configurable data/parity shards
- Pluggable storage backends via `Backend` trait (local disk today, cloud later)
- Bitrot detection via per-shard SHA256 checksums
- Background healing: MRF auto-enqueue on partial writes + periodic integrity scanner
- AWS Signature V4 authentication (or --no-auth for local use)
- Admin API: server status, backend health, healing status, object inspection, manual heal

**Not done:**
- Object tagging, versioning, bucket policies, lifecycle rules
- Multipart upload (required for files >5GB)
- Bucket replication
- Cloud storage backends (Backend trait is ready, implementations are not)

See [docs/s3-compliance.md](docs/s3-compliance.md) for full S3 API compliance audit (5/10 overall).

## License

[GNU General Public License v3.0](LICENSE)
