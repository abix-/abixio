# AbixIO

S3-compatible object store with erasure coding. Single Rust binary.

## What it does

Spreads your data across disks using Reed-Solomon erasure coding. Lose a disk,
lose nothing. Any S3 client works out of the box.

- 1 disk minimum, scale up by adding disks
- Per-object erasure coding (data/parity per object, bucket, or server default)
- Pluggable storage backends via `Backend` trait
- Per-shard SHA256 bitrot detection
- Background healing (MRF queue + integrity scanner)
- AWS Signature V4 authentication + presigned URL support
- Object versioning (enable/suspend per bucket)
- Object and bucket tagging
- Multipart upload (files of any size)
- Bucket policies and lifecycle configuration
- Admin API for disk health, healing status, shard inspection

## Quick start

```bash
cargo build --release

mkdir -p /tmp/abixio/{d1,d2,d3,d4}

./target/release/abixio --listen 0.0.0.0:9000 \
  --disks /tmp/abixio/d1,/tmp/abixio/d2,/tmp/abixio/d3,/tmp/abixio/d4 \
  --data 2 --parity 2 --no-auth
```

```bash
curl -X PUT http://localhost:9000/mybucket
curl -X PUT -d "hello world" http://localhost:9000/mybucket/hello.txt
curl http://localhost:9000/mybucket/hello.txt
```

Works with AWS CLI, rclone, MinIO client, boto3, or any S3-compatible tool.

## Configuration

Server defaults set the data/parity ratio for new objects. You can override
per bucket or per object.

| Config | Behavior |
|---|---|
| 1 disk, 1 data, 0 parity | Plain S3 storage. No redundancy. |
| 2 disks, 1 data, 1 parity | Mirror-like. Survives 1 disk failure. |
| 4 disks, 2 data, 2 parity | Erasure coding. Survives 2 failures. |
| 6 disks, 2 data, 4 parity | Heavy parity. Survives 4 failures. |
| 6 disks, 2 data, 2 parity | Disk pool. Default EC uses 4 of 6 disks per object. |

### Per-object erasure coding

Each object can have its own data/parity ratio. Set via S3 custom metadata headers:

```bash
# critical file: 1 data + 5 parity (survives 5 disk failures)
curl -X PUT -d "important" \
  -H "x-amz-meta-ec-data: 1" -H "x-amz-meta-ec-parity: 5" \
  http://localhost:9000/mybucket/critical.txt

# large file: 4 data + 2 parity (throughput-optimized)
curl -X PUT -d @bigfile.bin \
  -H "x-amz-meta-ec-data: 4" -H "x-amz-meta-ec-parity: 2" \
  http://localhost:9000/mybucket/bigfile.bin
```

Bucket-level defaults via admin API:

```bash
curl -X PUT "http://localhost:9000/_admin/bucket/mybucket/ec?data=3&parity=3"
```

Precedence: per-object header > bucket config > server default (`--data`/`--parity`).

## S3 API coverage

41 of 72 S3 API operations (57%). 196 tests.
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
| [storage-layout.md](docs/storage-layout.md) | Disk layout, meta.json format, erasure distribution |
| [versioning.md](docs/versioning.md) | S3 object versioning lifecycle |
| [tagging.md](docs/tagging.md) | Object and bucket tagging |
| [presigned-urls.md](docs/presigned-urls.md) | Presigned URL authentication |
| [conditional-requests.md](docs/conditional-requests.md) | If-Match, If-None-Match, etc. |
| [error-responses.md](docs/error-responses.md) | Error XML format, codes, request ID |
| [multipart-upload.md](docs/multipart-upload.md) | Multipart upload lifecycle and disk layout |
| [bucket-policy.md](docs/bucket-policy.md) | Bucket policy storage and validation |
| [healing.md](docs/healing.md) | Erasure healing, MRF queue, scanner |
| [s3-compliance.md](docs/s3-compliance.md) | Full S3 API compliance audit (all 72 operations) |

## Related

- **[abixio-ui](https://github.com/abix-/abixio-ui)** -- native desktop S3 manager and AbixIO admin UI. Browse, upload, manage objects. Auto-detects AbixIO servers for disk health, healing, and shard inspection.

## License

[GNU General Public License v3.0](LICENSE)
