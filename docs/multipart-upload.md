# Multipart Upload

How abixio implements S3 multipart upload. Required for files >5GB and used
by most S3 clients for files >64MB for parallel upload performance.

## S3 endpoints

| Endpoint | HTTP | Query | Description |
|---|---|---|---|
| CreateMultipartUpload | `POST /{bucket}/{key}?uploads` | | Returns upload ID |
| UploadPart | `PUT /{bucket}/{key}?partNumber=N&uploadId=X` | | Returns part ETag |
| CompleteMultipartUpload | `POST /{bucket}/{key}?uploadId=X` | XML body | Assembles final object |
| AbortMultipartUpload | `DELETE /{bucket}/{key}?uploadId=X` | | Cleans up upload state |
| ListParts | `GET /{bucket}/{key}?uploadId=X` | | Lists uploaded parts |
| ListMultipartUploads | `GET /{bucket}?uploads` | | Lists in-progress uploads |

## How it works

### 1. CreateMultipartUpload

Client sends `POST /{bucket}/{key}?uploads`. Server generates a UUID upload ID
and a UUID data directory, writes `upload.json` to all erasure disks.

Response:
```xml
<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>mybucket</Bucket>
  <Key>bigfile.tar</Key>
  <UploadId>550e8400-e29b-41d4-a716-446655440000</UploadId>
</InitiateMultipartUploadResult>
```

### 2. UploadPart

Client sends `PUT /{bucket}/{key}?partNumber=1&uploadId=X` with part data in the
body. Each part is individually erasure-encoded and its shards are distributed
across the multipart worker's current local disk view.

Per-part metadata (`part.N.meta`) records size, ETag (MD5), erasure info, and
shard checksum on each disk.

Part numbers do not need to be sequential. Re-uploading the same part number
overwrites the previous data for that part.

Response includes `ETag` header with MD5 of the part data.

### 3. CompleteMultipartUpload

Client sends `POST /{bucket}/{key}?uploadId=X` with XML body listing parts:
```xml
<CompleteMultipartUpload>
  <Part><PartNumber>1</PartNumber><ETag>"abc..."</ETag></Part>
  <Part><PartNumber>2</PartNumber><ETag>"def..."</ETag></Part>
</CompleteMultipartUpload>
```

Server validates the requested parts, moves each staged part shard and its
per-part metadata into the final object directory on each disk, then writes a
final `meta.json` containing the multipart parts manifest.

Current implementation note:

- multipart uses its own local planner (`multipart-local`) rather than the
  `VolumePool` placement topology used by normal object writes
- this means multipart does not yet preserve cluster placement metadata in the
  same way as the newer placement-aware object path

Final ETag follows S3 multipart format: `MD5(concat(part_etags))-N` where N
is the number of parts.

Upload state is cleaned up from all disks after successful completion.

### 4. AbortMultipartUpload

Deletes the upload directory and all part data from all disks. Returns 204.

### 5. ListParts

Returns XML listing all uploaded parts with their part number, size, and ETag.

```xml
<ListPartsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>mybucket</Bucket>
  <Key>bigfile.tar</Key>
  <UploadId>550e8400...</UploadId>
  <Part>
    <PartNumber>1</PartNumber>
    <ETag>"abc..."</ETag>
    <Size>5242880</Size>
  </Part>
</ListPartsResult>
```

### 6. ListMultipartUploads

Returns XML listing all in-progress uploads for a bucket.

```xml
<ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>mybucket</Bucket>
  <Upload>
    <Key>bigfile.tar</Key>
    <UploadId>550e8400...</UploadId>
    <Initiated>2024-01-01T00:00:00.000Z</Initiated>
  </Upload>
</ListMultipartUploadsResult>
```

## Disk layout

Upload state is stored under `.abixio.sys/multipart/` on each erasure disk:

```
.abixio.sys/
  multipart/
    <bucket>/
      <key>/
        <upload-id>/
          upload.json            # upload metadata
          <data-dir-uuid>/
            part.1               # erasure-encoded shard for part 1
            part.1.meta          # part metadata (size, etag, erasure info)
            part.2
            part.2.meta
```

Each part's shard data is distributed across disks using a deterministic local
multipart planner. This is similar in spirit to regular object erasure coding,
but it is currently separate from the newer placement-aware object path.

## Lifecycle

- In-progress uploads are invisible to GET and ListObjects
- Completed uploads become normal objects (appear in listings, support GET/HEAD/DELETE)
- Aborted uploads are fully cleaned up (no orphan data)
- Upload state is removed from ListMultipartUploads after complete or abort

## S3 spec limits

| Limit | Value |
|---|---|
| Min part size | 5MB (except last part) |
| Max part size | 5GB |
| Max parts per upload | 10,000 |
| Max object size | 5TB |

Note: abixio does not currently enforce these limits. Any part size works.

## Comparison with MinIO

| Aspect | MinIO | abixio |
|---|---|---|
| Upload state location | `.minio.sys/multipart/<sha>/<uuid>/` | `.abixio.sys/multipart/<bucket>/<key>/<uuid>/` |
| Metadata format | Binary msgpack (xl.meta) | JSON (upload.json + part.N.meta) |
| Part encoding | Erasure-encoded per part | Erasure-encoded per part |
| Upload ID | base64(deployment.uuid) | Plain UUID |
| Final assembly | References parts in-place | Moves staged part shards into final object directories and writes multipart metadata |
| Part size enforcement | Yes | Not yet |

## Client behavior

- **minio-go / mc**: Automatic multipart for files >= 64MB. Configurable part size.
- **aws-sdk-s3 (Rust)**: Manual multipart API. abixio-ui does not yet use it.
- **AWS CLI**: Automatic multipart for large files via `aws s3 cp`.

## Implementation

- `src/multipart/mod.rs`: all multipart state management and erasure encode/decode
- `src/s3_service.rs`: 6 S3 trait methods (create, upload_part, complete, abort, list_parts, list_uploads)
- s3s handles XML serialization of multipart responses via smithy-generated DTOs
- `tests/s3_integration.rs`: multipart integration coverage; use current `cargo test` output for exact counts

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| The six multipart S3 endpoints are implemented in `s3_service.rs` | Verified | `src/s3_service.rs:1004-1159`, `tests/s3_integration.rs` |
| CreateMultipartUpload writes `upload.json` with UUID upload ID and UUID data dir | Verified | `src/multipart/mod.rs:13-66` |
| UploadPart erasure-encodes each part and stores per-part metadata | Verified | `src/multipart/mod.rs:68-141` |
| Re-uploading the same part number overwrites the previous part data for that part number | Verified | `put_part()` writes fixed `part.N` / `part.N.meta` paths; test coverage in `tests/s3_integration.rs:1972-2003` |
| CompleteMultipartUpload reconstructs all parts and re-runs the normal PUT path | Corrected | Current code moves staged `part.N` files into the final object directory and writes multipart `meta.json`; it does not decode and re-encode the whole object (`src/multipart/mod.rs:186-366`) |
| Multipart uses its own local planner and fixed `volume_ids`/`epoch_id` metadata rather than the newer placement-aware object path | Verified | `src/multipart/mod.rs:83-126` |
| Completed multipart objects are represented as normal object directories with multipart part manifest metadata | Verified | `src/multipart/mod.rs:270-352`, `src/storage/metadata.rs:is_multipart`, `src/storage/volume_pool.rs:450-515` |
| S3 multipart size limits are documented but not currently enforced | Verified | No limit enforcement appears in `src/multipart/mod.rs`; tests cover empty/single/small parts in `tests/s3_integration.rs` |
| The MinIO comparison row claiming abixio "reads + concatenates + re-encodes" was accurate | Corrected | Current final assembly no longer matches that description (`src/multipart/mod.rs`) |

Verdict: multipart support is real and well covered, but the page had drifted behind the implementation. The biggest correction is that completion now moves staged part shards into place and writes multipart metadata instead of rebuilding the entire object through the normal PUT path.
