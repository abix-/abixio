# AbixIO Server

## Why AbixIO

MinIO and its closest descendants (RustFS) require identical nodes within a pool and fix parity at the erasure-set level. AbixIO drops both constraints.

**Per-object fault tolerance.** Your critical config file gets 1+5 erasure coding. Your bulk logs get 3+1. Same bucket, same disks. Arbitrary data/parity ratios set inline per object via headers.

**Heterogeneous everything.** Mixed disk counts, mixed node sizes, mixed volume types. One flat pool. Think vSAN meets S3.

**One node to many.** Single disk, six disks, three nodes. Same storage model. Node count changes the resilience envelope, not the product.

Early. Foundation works. Vision is what to watch.

## Status

**Early development. Not production-ready.**

This repository is the AbixIO server: an S3-compatible object storage server written in Rust with erasure coding, multi-volume storage, and node-based clustering.

Current reality as of 2026-04-05:
- First commit: 2026-04-04
- No releases, packaging, or production deployments
- 316 automated tests (unit + integration)
- 41 of 72 S3 API operations implemented (57%)
- Core storage engine works with real S3 clients, but several important gaps remain

What works today:
- Erasure-coded object storage across local and remote volumes
- Internode shard RPC over HTTP with JWT authentication
- Multi-node cluster formation and identity exchange
- Quorum-aware fencing when nodes become unsafe
- Deterministic placement metadata in object shards
- Background healing with MRF queue and integrity scanner
- Object versioning, tagging, multipart upload, and presigned URLs
- Bucket policy and lifecycle configuration storage

What is still missing or broken:
- No production field time, benchmarks, load testing, or failure-injection testing
- Object tagging returns errors in the latest build
- Chunked transfer encoding is not properly stripped in relay paths
- Bucket delete fails when versioned objects exist
- No encryption at rest
- No live topology changes or rebalance
- No consensus-backed control plane such as Raft
- Cluster mode is covered by integration tests but not battle-tested at scale

If you need a production S3-compatible object store today, start with [RustFS](https://github.com/rustfs/rustfs). It is much closer to a MinIO-style design. [SeaweedFS](https://github.com/seaweedfs/seaweedfs) is also a strong option if its model fits your workload better.

## What AbixIO Server Is

AbixIO Server is a single Rust binary that exposes an S3-compatible API and stores objects across one or more volumes using Reed-Solomon erasure coding.

Key properties:
- Server, not client: this repo is the storage server itself
- Any S3 client can talk to it: AWS CLI, rclone, boto3, MinIO client, custom tools
- Single-node through multi-node deployment under one storage model
- Self-describing volumes store their own identity metadata
- Cluster membership is node-based and discovered from shared startup configuration
- Per-object SHA256 checksums detect shard corruption
- Healing runs in the background when damaged or missing shards are found

## Features

- 1 volume minimum, scale up by adding volumes
- Single-node and clustered deployments are both first-class operating modes
- Per-object erasure coding with FTT (failures to tolerate) and bucket defaults
- Pluggable storage backends via the `Backend` trait
- Self-describing volumes with identity stored on disk
- Node-based cluster formation with no master server
- Background healing and integrity scanning
- AWS Signature V4 authentication and presigned URL support
- Object versioning
- Object and bucket tagging
- Multipart upload with per-part erasure coding (parts stay separate on disk)
- Bucket policy and lifecycle configuration storage
- Admin API for volume health, healing status, shard inspection, and bucket EC config
- Thick-console administration model for serious multi-node visibility, rather than depending only on a thin per-node web UI
- Quorum-aware readiness and hard fencing

## Quick Start

```bash
cargo build --release

mkdir -p /tmp/abixio/{d1,d2,d3,d4}

./target/release/abixio \
  --volumes /tmp/abixio/d{1...4} \
  --no-auth
```

With 4 volumes, the server auto-computes `3+1` erasure coding, which tolerates 1 volume failure.

```bash
curl -X PUT http://localhost:10000/mybucket
curl -X PUT -d "hello world" http://localhost:10000/mybucket/hello.txt
curl http://localhost:10000/mybucket/hello.txt
```

Any S3-compatible client can use the server once it is running.

## Cluster Mode

Each node receives the same `--nodes` list containing the full cluster membership. A node determines its own identity automatically at startup.

```bash
# multi-node cluster
abixio --volumes /data{1...2} --nodes http://node{1...3}:10000

# multiple nodes on one machine
abixio --volumes /data{1...2} --nodes http://localhost:{10001...10003}
```

Startup flow:
1. Read `.abixio.sys/volume.json` from attached volumes
2. On first boot, generate `node_id` and `volume_id` values and write them to disk
3. Exchange identity with peers through `/_admin/cluster/join`
4. Wait for cluster membership confirmation
5. Begin serving traffic

On later boots, the node reloads identity from disk and checks quorum. If quorum is lost, the node fences itself.

## CLI Flags

| Flag | Required | Default | Purpose |
|---|---|---|---|
| `--volumes` | yes | -- | Volume paths (comma-separated, supports `{N...M}`) |
| `--listen` | no | `:10000` | Bind address |
| `--nodes` | no | empty | All node endpoints (comma-separated, supports `{N...M}`) |
| `--no-auth` | no | false | Disable all authentication |

## Erasure Coding

Every bucket has an FTT (failures to tolerate) setting. FTT is the number of volume failures an object can survive. New buckets default to FTT=1. The system computes optimal data/parity from FTT and disk count.

| Volumes | Bucket FTT | Auto EC | Behavior |
|---|---|---|---|
| 1 | 0 | `1+0` | Plain object storage. No redundancy. |
| 2 | 1 | `1+1` | Mirror. Survives 1 failure. |
| 4 | 1 | `3+1` | Erasure coding. Survives 1 failure. |
| 6 | 1 | `5+1` | Erasure coding. Survives 1 failure. |

Override per object with an FTT header:

```bash
curl -X PUT -d "important" \
  -H "x-amz-meta-ec-ftt: 2" \
  http://localhost:10000/mybucket/critical.txt
```

Override per bucket through the admin API:

```bash
curl -X PUT "http://localhost:10000/_admin/bucket/mybucket/ftt?ftt=2"
```

Precedence: per-object FTT > bucket FTT.

## S3 API Coverage

41 of 72 S3 API operations are implemented (57%). Full details: [docs/s3-compliance.md](docs/s3-compliance.md)

Implemented well enough to use:
- ListBuckets, CreateBucket, HeadBucket, DeleteBucket
- ListObjectsV2
- PutObject, GetObject, HeadObject, DeleteObject, DeleteObjects, CopyObject
- Get/Put/DeleteObjectTagging
- Get/Put/DeleteBucketTagging
- PutBucketVersioning, GetBucketVersioning, ListObjectVersions
- CreateMultipartUpload, UploadPart, CompleteMultipartUpload
- AbortMultipartUpload, ListParts, ListMultipartUploads

Stored but not enforced:
- Get/Put/DeleteBucketPolicy
- Get/Put/DeleteBucketLifecycle

Stubbed for compatibility:
- Get/Put/DeleteBucketCors
- Get/PutBucketACL
- Get/PutObjectACL
- Get/PutBucketNotification

Not implemented:
- Encryption config
- Replication
- Object lock and retention
- S3 Select
- Cloud storage backends

## Documentation

| Doc | Subject |
|---|---|
| [architecture.md](docs/architecture.md) | Design principles, cluster direction, project structure |
| [comparison.md](docs/comparison.md) | Competitor overlap, differentiation, and capability matrix |
| [cluster.md](docs/cluster.md) | Cluster control design, quorum model, fencing |
| [storage-layout.md](docs/storage-layout.md) | On-disk metadata layers, volume identity, directory structure |
| [versioning.md](docs/versioning.md) | S3 object versioning lifecycle |
| [tagging.md](docs/tagging.md) | Object and bucket tagging |
| [presigned-urls.md](docs/presigned-urls.md) | Presigned URL authentication |
| [conditional-requests.md](docs/conditional-requests.md) | If-Match, If-None-Match, and related behavior |
| [error-responses.md](docs/error-responses.md) | Error XML format, codes, request ID |
| [multipart-upload.md](docs/multipart-upload.md) | Multipart upload lifecycle and disk layout |
| [bucket-policy.md](docs/bucket-policy.md) | Bucket policy storage and validation |
| [per-object-ec.md](docs/per-object-ec.md) | Per-object erasure coding, bucket EC config, volume pools |
| [healing.md](docs/healing.md) | Erasure healing, MRF queue, scanner |
| [s3-compliance.md](docs/s3-compliance.md) | Full S3 API compliance audit |

## Related Project

- **[abixio-ui](https://github.com/abix-/abixio-ui)**: desktop S3 manager and AbixIO admin UI for connecting to an AbixIO server

## License

[GNU General Public License v3.0](LICENSE)
