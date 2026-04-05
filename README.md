# AbixIO

S3-compatible object store with erasure coding. Single Rust binary, node-based
clustering, self-describing volumes.

## What it does

Spreads your data across volumes using Reed-Solomon erasure coding. Lose a
volume, lose nothing. Any S3 client works out of the box.

- 1 volume minimum, scale up by adding volumes
- Per-object erasure coding (data/parity per object, bucket, or server default)
- Pluggable storage backends via `Backend` trait (local volumes + internode RPC)
- Self-describing volumes -- identity stored on the volumes themselves
- Node-based cluster formation -- no topology file, no master server
- Per-shard SHA256 bitrot detection
- Background healing (MRF queue + integrity scanner)
- AWS Signature V4 authentication + presigned URL support
- Object versioning (enable/suspend per bucket)
- Object and bucket tagging
- Multipart upload (files of any size)
- Bucket policy and lifecycle configuration storage
- Admin API for volume health, healing status, shard inspection, bucket EC config
- Quorum-aware readiness and hard fencing

## Status

**Alpha.** Erasure-coded storage with internode shard RPC is implemented and
tested (262 tests). The S3 API covers the core operations (57% of the spec).

What works today:
- Erasure-coded object storage across local and remote volumes
- Internode shard RPC over HTTP with JWT authentication
- Multi-node cluster formation and identity exchange
- Quorum-aware fencing (unsafe nodes stop serving)
- Deterministic placement metadata in object shards
- Background healing (MRF queue + integrity scanner)

What does not work yet:
- Live topology changes or rebalance
- Consensus-backed control plane (Raft or equivalent)
- Encryption at rest

## Quick start

```bash
cargo build --release

mkdir -p /tmp/abixio/{d1,d2,d3,d4}

./target/release/abixio \
  --volumes /tmp/abixio/d{1...4} \
  --no-auth
```

With 4 volumes, the server auto-computes `3+1` EC (1-failure tolerance).
Override per bucket or per object.

```bash
curl -X PUT http://localhost:10000/mybucket
curl -X PUT -d "hello world" http://localhost:10000/mybucket/hello.txt
curl http://localhost:10000/mybucket/hello.txt
```

Works with AWS CLI, rclone, MinIO client, boto3, or any S3-compatible tool.

## Cluster mode

Every node gets the same `--nodes` list -- the full cluster membership. Each
node figures out which one it is automatically.

```bash
# multi-node cluster
abixio --volumes /data{1...2} --nodes http://node{1...3}:10000

# multiple nodes on the same server (different ports)
abixio --volumes /data{1...2} --nodes http://localhost:{10001...10003}
```

What happens at startup:

1. Each node reads `.abixio.sys/volume.json` from its volumes
2. If first boot, generates node_id and volume_id UUIDs, writes volume.json
3. Exchanges identity with other nodes via `/_admin/cluster/join`
4. Blocks until all nodes respond, then finalizes volume.json with full membership
5. Serves traffic

On subsequent boots, identity is read from volume.json. Nodes are probed for
quorum confirmation. If quorum is lost, the node fences itself.

### CLI flags

| Flag | Required | Default | Purpose |
|---|---|---|---|
| `--volumes` | yes | -- | Volume paths (comma-separated, supports `{N...M}`) |
| `--listen` | no | `:10000` | Bind address |
| `--nodes` | no | empty | All node endpoints (comma-separated, supports `{N...M}`) |
| `--cluster-secret` | no | empty | Shared secret for node probes |
| `--no-auth` | no | false | Disable S3 authentication |

EC defaults are auto-computed from volume count: 1 volume = `1+0` (no
redundancy), 2+ volumes = `(N-1)+1` (1-failure tolerance). Override per
bucket via admin API or per object via S3 headers.

## Configuration

EC defaults are auto-computed from the volume count to achieve 1-failure
tolerance. Override per bucket or per object.

| Volumes | Auto EC | Behavior |
|---|---|---|
| 1 | `1+0` | Plain S3 storage. No redundancy. |
| 2 | `1+1` | Mirror. Survives 1 failure. |
| 4 | `3+1` | Erasure coding. Survives 1 failure. |
| 6 | `5+1` | Erasure coding. Survives 1 failure. |

For different ratios, set per-bucket EC via the admin API:

```bash
# 6 volumes, want 2+2 instead of auto 5+1
curl -X PUT "http://localhost:10000/_admin/bucket/mybucket/ec?data=2&parity=2"
```

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

Precedence: per-object header > bucket config > auto-computed server default.

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
