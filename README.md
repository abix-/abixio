# AbixIO

S3-compatible object store with erasure coding. Single Rust binary, peer-based
clustering, self-describing volumes.

## What it does

Spreads your data across volumes using Reed-Solomon erasure coding. Lose a
volume, lose nothing. Any S3 client works out of the box.

- 1 volume minimum, scale up by adding volumes
- Per-object erasure coding (data/parity per object, bucket, or server default)
- Pluggable storage backends via `Backend` trait (local disk, NFS, S3, anything)
- Self-describing volumes -- identity stored on the volumes themselves
- Peer-based cluster formation -- no topology file, no master server
- Per-shard SHA256 bitrot detection
- Background healing (MRF queue + integrity scanner)
- AWS Signature V4 authentication + presigned URL support
- Object versioning (enable/suspend per bucket)
- Object and bucket tagging
- Multipart upload (files of any size)
- Bucket policy and lifecycle configuration storage
- Admin API for volume health, healing status, shard inspection, bucket EC config
- Quorum-aware readiness and hard fencing

## Quick start

```bash
cargo build --release

mkdir -p /tmp/abixio/{d1,d2,d3,d4}

./target/release/abixio \
  --disks /tmp/abixio/d1,/tmp/abixio/d2,/tmp/abixio/d3,/tmp/abixio/d4 \
  --data 2 --parity 2 --no-auth
```

```bash
curl -X PUT http://localhost:10000/mybucket
curl -X PUT -d "hello world" http://localhost:10000/mybucket/hello.txt
curl http://localhost:10000/mybucket/hello.txt
```

Works with AWS CLI, rclone, MinIO client, boto3, or any S3-compatible tool.

## Cluster mode

Nodes discover each other via `--peers`. No topology file needed. Each node
generates its own identity on first boot, exchanges it with peers, and builds
the erasure set membership from the handshake.

```bash
# node 1
./target/release/abixio \
  --disks /srv/abixio/d1,/srv/abixio/d2 \
  --peers http://node-2:10000,http://node-3:10000 \
  --data 2 --parity 2

# node 2
./target/release/abixio \
  --disks /srv/abixio/d1,/srv/abixio/d2 \
  --peers http://node-1:10000,http://node-3:10000 \
  --data 2 --parity 2

# node 3
./target/release/abixio \
  --disks /srv/abixio/d1,/srv/abixio/d2 \
  --peers http://node-1:10000,http://node-2:10000 \
  --data 2 --parity 2
```

What happens at startup:

1. Each node reads `.abixio.sys/volume.json` from its volumes
2. If first boot, generates node_id and volume_id UUIDs, writes volume.json
3. Exchanges identity with peers via `/_admin/cluster/join`
4. Blocks until all peers respond, then finalizes volume.json with full membership
5. Serves traffic

On subsequent boots, identity is read from volume.json. Peers are probed for
quorum confirmation. If quorum is lost, the node fences itself.

### CLI flags

| Flag | Required | Default | Purpose |
|---|---|---|---|
| `--disks` | yes | -- | Comma-separated volume paths |
| `--data` | no | 1 | Server default data shards |
| `--parity` | no | 0 | Server default parity shards |
| `--listen` | no | `:10000` | Bind address |
| `--peers` | no | empty | Peer endpoints for cluster mode |
| `--cluster-secret` | no | empty | Shared secret for peer probes |
| `--no-auth` | no | false | Disable S3 authentication |

## Configuration

Server defaults set the data/parity ratio for new objects. You can override
per bucket or per object.

| Config | Behavior |
|---|---|
| 1 volume, 1 data, 0 parity | Plain S3 storage. No redundancy. |
| 2 volumes, 1 data, 1 parity | Mirror-like. Survives 1 volume failure. |
| 4 volumes, 2 data, 2 parity | Erasure coding. Survives 2 failures. |
| 6 volumes, 2 data, 4 parity | Heavy parity. Survives 4 failures. |
| 6 volumes, 2 data, 2 parity | Volume pool. Default EC uses 4 of 6 volumes per object. |

### Per-object erasure coding

Each object can have its own data/parity ratio. Set via S3 custom metadata headers:

```bash
# critical file: 1 data + 5 parity (survives 5 volume failures)
curl -X PUT -d "important" \
  -H "x-amz-meta-ec-data: 1" -H "x-amz-meta-ec-parity: 5" \
  http://localhost:10000/mybucket/critical.txt

# large file: 4 data + 2 parity (throughput-optimized)
curl -X PUT -d @bigfile.bin \
  -H "x-amz-meta-ec-data: 4" -H "x-amz-meta-ec-parity: 2" \
  http://localhost:10000/mybucket/bigfile.bin
```

Bucket-level defaults via admin API:

```bash
curl -X PUT "http://localhost:10000/_admin/bucket/mybucket/ec?data=3&parity=3"
```

Precedence: per-object header > bucket config > server default (`--data`/`--parity`).

## S3 API coverage

41 of 72 S3 API operations (57%).
See [docs/s3-compliance.md](docs/s3-compliance.md) for the full audit.

**Fully implemented (26):** ListBuckets, CreateBucket, HeadBucket, DeleteBucket,
ListObjectsV2, PutObject, GetObject, HeadObject, DeleteObject, DeleteObjects,
CopyObject, Get/Put/DeleteObjectTagging, Get/Put/DeleteBucketTagging,
PutBucketVersioning, GetBucketVersioning, ListObjectVersions,
CreateMultipartUpload, UploadPart, CompleteMultipartUpload,
AbortMultipartUpload, ListParts, ListMultipartUploads.

**Stored but not enforced (6):** Get/Put/DeleteBucketPolicy,
Get/Put/DeleteBucketLifecycle.

**Stubs matching MinIO (9):** Get/Put/DeleteBucketCors (501 NotImplemented),
Get/PutBucketACL, Get/PutObjectACL (hardcoded FULL_CONTROL),
Get/PutBucketNotification (empty config / 501).

**Not implemented:** Encryption config, replication, object lock/retention,
S3 Select, cloud storage backends.

## Documentation

| Doc | Subject |
|---|---|
| [architecture.md](docs/architecture.md) | Design principles, project structure, MinIO comparison |
| [cluster.md](docs/cluster.md) | Cluster control design, quorum model, fencing |
| [storage-layout.md](docs/storage-layout.md) | On-disk metadata layers, volume identity, directory structure |
| [versioning.md](docs/versioning.md) | S3 object versioning lifecycle |
| [tagging.md](docs/tagging.md) | Object and bucket tagging |
| [presigned-urls.md](docs/presigned-urls.md) | Presigned URL authentication |
| [conditional-requests.md](docs/conditional-requests.md) | If-Match, If-None-Match, etc. |
| [error-responses.md](docs/error-responses.md) | Error XML format, codes, request ID |
| [multipart-upload.md](docs/multipart-upload.md) | Multipart upload lifecycle and disk layout |
| [bucket-policy.md](docs/bucket-policy.md) | Bucket policy storage and validation |
| [per-object-ec.md](docs/per-object-ec.md) | Per-object erasure coding, bucket EC config, volume pools |
| [healing.md](docs/healing.md) | Erasure healing, MRF queue, scanner |
| [s3-compliance.md](docs/s3-compliance.md) | Full S3 API compliance audit (all 72 operations) |

## Related

- **[abixio-ui](https://github.com/abix-/abixio-ui)** -- native desktop S3 manager and AbixIO admin UI. Browse, upload, manage objects. Auto-detects AbixIO servers for volume health, healing, and shard inspection.

## License

[GNU General Public License v3.0](LICENSE)
