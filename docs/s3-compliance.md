# S3 API Compliance

Authoritative audit of every S3 API operation. Compared against MinIO's
72 unique handler routes in `cmd/api-router.go`.

abixio implements **41 of 72** operations.

## All operations

Status: Done = implemented and tested, No = not implemented, N/A = out of scope.

### Bucket operations

| Operation | HTTP | Status | Rating | Notes |
|---|---|---|---|---|
| ListBuckets | `GET /` | Done | 8/10 | XML with names and creation dates. |
| CreateBucket | `PUT /{bucket}` | Done | 7/10 | No region, ACL, or object lock config. |
| HeadBucket | `HEAD /{bucket}` | Done | 8/10 | Returns 200 or 404. |
| DeleteBucket | `DELETE /{bucket}` | Done | 8/10 | 409 if not empty. |
| GetBucketLocation | `GET /{bucket}?location` | No | 0/10 | |
| GetBucketVersioning | `GET /{bucket}?versioning` | Done | 8/10 | Returns Enabled/Suspended. |
| PutBucketVersioning | `PUT /{bucket}?versioning` | Done | 8/10 | Enable or suspend per bucket. |
| GetBucketTagging | `GET /{bucket}?tagging` | Done | 8/10 | |
| PutBucketTagging | `PUT /{bucket}?tagging` | Done | 8/10 | |
| DeleteBucketTagging | `DELETE /{bucket}?tagging` | Done | 8/10 | |
| GetBucketPolicy | `GET /{bucket}?policy` | Done | 8/10 | Returns JSON. 404 if none set. |
| PutBucketPolicy | `PUT /{bucket}?policy` | Done | 8/10 | Validates JSON + Version. Max 20KiB. |
| DeleteBucketPolicy | `DELETE /{bucket}?policy` | Done | 8/10 | Idempotent. |
| GetBucketEncryption | `GET /{bucket}?encryption` | No | 0/10 | |
| PutBucketEncryption | `PUT /{bucket}?encryption` | No | 0/10 | |
| DeleteBucketEncryption | `DELETE /{bucket}?encryption` | No | 0/10 | |
| GetBucketLifecycle | `GET /{bucket}?lifecycle` | Done | 8/10 | Returns stored XML. 404 if none. |
| PutBucketLifecycle | `PUT /{bucket}?lifecycle` | Done | 7/10 | Stores XML. Basic validation. No enforcement. |
| DeleteBucketLifecycle | `DELETE /{bucket}?lifecycle` | Done | 8/10 | Idempotent. |
| GetBucketCors | `GET /{bucket}?cors` | Stub | 3/10 | Returns 404 NoSuchCORSConfiguration (matches MinIO). 2.0 item. |
| PutBucketCors | `PUT /{bucket}?cors` | Stub | 3/10 | Returns 501 NotImplemented (matches MinIO). 2.0 item. |
| DeleteBucketCors | `DELETE /{bucket}?cors` | Stub | 3/10 | Returns 501 NotImplemented (matches MinIO). 2.0 item. |
| GetBucketACL | `GET /{bucket}?acl` | Stub | 3/10 | Returns hardcoded FULL_CONTROL (matches MinIO). |
| PutBucketACL | `PUT /{bucket}?acl` | Stub | 3/10 | Accepts private only, rejects others with 501 (matches MinIO). |
| GetBucketReplication | `GET /{bucket}?replication` | No | 0/10 | |
| PutBucketReplication | `PUT /{bucket}?replication` | No | 0/10 | |
| DeleteBucketReplication | `DELETE /{bucket}?replication` | No | 0/10 | |
| GetBucketNotification | `GET /{bucket}?notification` | Stub | 3/10 | Returns empty config XML. 2.0 item. |
| PutBucketNotification | `PUT /{bucket}?notification` | Stub | 3/10 | Returns 501. 2.0 item. |
| GetBucketLogging | `GET /{bucket}?logging` | No | 0/10 | |
| GetBucketWebsite | `GET /{bucket}?website` | No | 0/10 | |
| DeleteBucketWebsite | `DELETE /{bucket}?website` | No | 0/10 | |
| GetBucketObjectLockConfig | `GET /{bucket}?object-lock` | No | 0/10 | |
| PutBucketObjectLockConfig | `PUT /{bucket}?object-lock` | No | 0/10 | |
| GetBucketAccelerate | `GET /{bucket}?accelerate` | N/A | n/a | AWS-specific. |
| GetBucketRequestPayment | `GET /{bucket}?requestPayment` | N/A | n/a | AWS-specific. |
| GetBucketPolicyStatus | `GET /{bucket}?policyStatus` | No | 0/10 | |

### Object listing

| Operation | HTTP | Status | Rating | Notes |
|---|---|---|---|---|
| ListObjectsV2 | `GET /{bucket}` | Done | 7/10 | Prefix, delimiter, max-keys, pagination. |
| ListObjectVersions | `GET /{bucket}?versions` | Done | 8/10 | Versions + delete markers. |
| ListMultipartUploads | `GET /{bucket}?uploads` | Done | 8/10 | In-progress uploads. |

### Object operations

| Operation | HTTP | Status | Rating | Notes |
|---|---|---|---|---|
| PutObject | `PUT /{bucket}/{key}` | Done | 9/10 | Content-type, custom metadata, versioning, per-object EC via x-amz-meta headers. |
| GetObject | `GET /{bucket}/{key}` | Done | 9/10 | Range, conditionals, version-id. |
| HeadObject | `HEAD /{bucket}/{key}` | Done | 9/10 | Conditionals, custom metadata. |
| DeleteObject | `DELETE /{bucket}/{key}` | Done | 8/10 | Delete markers for versioned buckets. |
| DeleteObjects | `POST /{bucket}?delete` | Done | 8/10 | Batch delete up to 1000 keys. |
| CopyObject | `PUT /{bucket}/{key}` (x-amz-copy-source) | Done | 7/10 | Same-bucket and cross-bucket. GET+PUT internally. |
| GetObjectTagging | `GET /{bucket}/{key}?tagging` | Done | 8/10 | |
| PutObjectTagging | `PUT /{bucket}/{key}?tagging` | Done | 8/10 | |
| DeleteObjectTagging | `DELETE /{bucket}/{key}?tagging` | Done | 8/10 | |
| GetObjectACL | `GET /{bucket}/{key}?acl` | Stub | 3/10 | Returns hardcoded FULL_CONTROL (matches MinIO). |
| PutObjectACL | `PUT /{bucket}/{key}?acl` | Stub | 3/10 | Accepts private only (matches MinIO). |
| GetObjectRetention | `GET /{bucket}/{key}?retention` | No | 0/10 | |
| PutObjectRetention | `PUT /{bucket}/{key}?retention` | No | 0/10 | |
| GetObjectLegalHold | `GET /{bucket}/{key}?legal-hold` | No | 0/10 | |
| PutObjectLegalHold | `PUT /{bucket}/{key}?legal-hold` | No | 0/10 | |
| GetObjectAttributes | `GET /{bucket}/{key}?attributes` | No | 0/10 | |
| PostRestoreObject | `POST /{bucket}/{key}?restore` | No | 0/10 | Glacier restore. |
| SelectObjectContent | `POST /{bucket}/{key}?select` | No | 0/10 | S3 Select (SQL). |
| PostPolicyBucket | `POST /{bucket}` | No | 0/10 | Browser-based upload. |

### Multipart upload

| Operation | HTTP | Status | Rating | Notes |
|---|---|---|---|---|
| CreateMultipartUpload | `POST /{bucket}/{key}?uploads` | Done | 8/10 | Returns upload ID. |
| UploadPart | `PUT /{bucket}/{key}?partNumber&uploadId` | Done | 8/10 | Erasure-encoded per part. |
| CompleteMultipartUpload | `POST /{bucket}/{key}?uploadId` | Done | 8/10 | Assembles + writes final object. |
| AbortMultipartUpload | `DELETE /{bucket}/{key}?uploadId` | Done | 8/10 | Full cleanup. |
| ListParts | `GET /{bucket}/{key}?uploadId` | Done | 8/10 | |
| CopyObjectPart | `PUT /{bucket}/{key}?partNumber&uploadId` (x-amz-copy-source) | No | 0/10 | Server-side part copy. |

### MinIO extensions (not standard S3)

| Operation | Status | Notes |
|---|---|---|
| ListObjectVersionsM (metadata) | No | MinIO extension. |
| PutObjectExtract | No | MinIO tar auto-extract. |
| GetObjectLambda | No | MinIO lambda. |
| ListenNotification | No | MinIO SSE notifications. |
| ResetBucketReplication | No | MinIO replication admin. |
| ValidateBucketReplicationCreds | No | MinIO replication admin. |
| GetBucketReplicationMetrics | No | MinIO replication metrics. |

## Response quality

s3s generates all XML responses from smithy models (AWS spec-compliant).

### Common response headers

| Header | Status |
|---|---|
| `x-amz-request-id` | Set on every response (UUID). |
| `Content-Type` | Set by s3s on all XML and object responses. |
| `ETag` | Set on PUT, GET, HEAD. RFC 7232 strong entity tag. |
| `Last-Modified` | RFC 7231 HTTP-date on GET, HEAD. |
| `x-amz-version-id` | Pending (versioning response headers not yet wired). |
| `x-amz-delete-marker` | Pending (versioning response headers not yet wired). |

### Error responses

s3s generates spec-compliant error XML with all standard S3 error codes.

| Field | Status |
|---|---|
| `<Code>` | All standard S3 error codes (smithy-generated). |
| `<Message>` | Descriptive messages. |
| `<RequestId>` | Set by dispatch layer (UUID). |
| `<Resource>` | Set by s3s when context available. |

## Auth

Protocol layer powered by [s3s](https://crates.io/crates/s3s) v0.13 (smithy-generated).

| Method | Status | Rating |
|---|---|---|
| SigV4 header auth | Done | 9/10 |
| SigV4 presigned URL | Done | 9/10 |
| SigV4 chunked transfer | Done | 9/10 |
| SigV4 trailing checksums | Done | 9/10 |
| POST policy uploads | Done | 8/10 |
| Content-MD5 validation | Done | 9/10 |
| Anonymous (no-auth mode) | Done | 8/10 |

SigV4 chunked transfer, trailing checksums, POST policy, and content-MD5
are handled by s3s at the protocol layer. No application code needed.

## Summary

41 of 72 operations (57%).

Coverage is backed by unit tests, S3 integration tests, admin integration
tests, and distributed placement tests. Exact test counts change over time and
should be taken from the current `cargo test` output rather than hardcoded
here.

Not implemented: encryption config, replication, notifications,
object lock/retention, ACLs, S3 Select.
CORS is stub-only. Full implementation is a 2.0 item.
