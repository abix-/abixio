# S3 API Compliance

How well abixio implements the S3 API. This is the authoritative doc for
what the server supports. The abixio-ui client repo references this doc
but does not duplicate it.

Ratings are 1-10 where 10 means "fully implemented, matches AWS S3 behavior"
and 1 means "not implemented at all."

## Implemented Endpoints

These are routed in `src/s3/handlers.rs` dispatch table.

| S3 API | HTTP | Route | Rating | Assessment |
|---|---|---|---|---|
| ListBuckets | `GET /` | `list_buckets` | 8/10 | Works. Returns XML with bucket names and real creation dates from filesystem. |
| CreateBucket | `PUT /{bucket}` | `create_bucket` | 7/10 | Works. No region, ACL, or object lock configuration support. |
| HeadBucket | `HEAD /{bucket}` | `head_bucket` | 8/10 | Works. Returns 200 or 404. |
| ListObjectsV2 | `GET /{bucket}` | `list_objects_handler` | 7/10 | Supports prefix, delimiter, max-keys. Has pagination (continuation token). Missing: list-type=2 query validation, encoding-type, start-after, fetch-owner. |
| PutObject | `PUT /{bucket}/{key}` | `put_object` | 8/10 | Reads full body, stores with content-type and custom metadata (x-amz-meta-*). Returns ETag. Missing: content-MD5 validation, storage class, tagging headers. |
| GetObject | `GET /{bucket}/{key}` | `get_object` | 8/10 | Returns body with Content-Type, Content-Length, ETag, Last-Modified (RFC 7231), Accept-Ranges, custom metadata. Supports Range requests (206 Partial Content). Missing: If-Match/If-None-Match conditionals. |
| HeadObject | `HEAD /{bucket}/{key}` | `head_object` | 8/10 | Returns Content-Type, Content-Length, ETag, Last-Modified (RFC 7231), Accept-Ranges, custom metadata. Missing: storage class, version ID, encryption info. |
| DeleteObject | `DELETE /{bucket}/{key}` | `delete_object` | 8/10 | Returns 204 No Content. Missing: version ID support, MFA delete. |
| DeleteBucket | `DELETE /{bucket}` | `delete_bucket_handler` | 8/10 | Returns 204 No Content. Only deletes empty buckets (returns 409 BucketNotEmpty otherwise). Matches S3 spec. |
| DeleteObjects (batch) | `POST /{bucket}?delete` | `delete_objects` | 8/10 | Parses XML request body, deletes each key, returns XML with deleted/error results. Up to 1000 keys per request. |
| CopyObject | `PUT /{bucket}/{key}` (x-amz-copy-source) | `copy_object` | 7/10 | Detects x-amz-copy-source header on PUT. Reads source, writes to destination with same content-type. Returns CopyObjectResult XML with ETag and LastModified. Currently does GET+PUT internally (no zero-copy optimization). |

## Not Implemented -- Medium Priority

| S3 API | HTTP | Rating | Impact |
|---|---|---|---|
| GetObjectTagging | `GET /{bucket}/{key}?tagging` | 8/10 | Returns XML TagSet from object metadata. |
| PutObjectTagging | `PUT /{bucket}/{key}?tagging` | 8/10 | Parses XML TagSet, stores in object metadata on all shards. |
| DeleteObjectTagging | `DELETE /{bucket}/{key}?tagging` | 8/10 | Clears tags from object metadata on all shards. |
| ListObjectVersions | `GET /{bucket}?versions` | 1/10 | No versioning support. Cannot list object versions. |
| GetBucketVersioning | `GET /{bucket}?versioning` | 1/10 | Cannot check if versioning is enabled. |
| PutBucketVersioning | `PUT /{bucket}?versioning` | 1/10 | Cannot enable/disable versioning. |
| GetBucketPolicy | `GET /{bucket}?policy` | 1/10 | No policy support. |
| PutBucketPolicy | `PUT /{bucket}?policy` | 1/10 | Same. |
| DeleteBucketPolicy | `DELETE /{bucket}?policy` | 1/10 | Same. |
| GetBucketEncryption | `GET /{bucket}?encryption` | 1/10 | No encryption config support. |
| PutBucketEncryption | `PUT /{bucket}?encryption` | 1/10 | Same. |
| GetBucketTagging | `GET /{bucket}?tagging` | 8/10 | Returns bucket tags from .tagging.json. |
| PutBucketTagging | `PUT /{bucket}?tagging` | 8/10 | Parses XML TagSet, stores as .tagging.json in bucket dir. |
| DeleteBucketTagging | `DELETE /{bucket}?tagging` | 8/10 | Removes .tagging.json from bucket dir. |

## Not Implemented -- Low Priority / Out of Scope

These are S3 features that self-hosted object storage servers commonly skip.

| Category | Operations | Rating | Notes |
|---|---|---|---|
| Multipart upload | CreateMultipartUpload, UploadPart, CompleteMultipartUpload, AbortMultipartUpload, ListMultipartUploads, ListParts | 1/10 | Required for files >5GB. Should implement eventually. |
| Object lock / retention | GetObjectRetention, PutObjectRetention, GetObjectLegalHold, PutObjectLegalHold, GetObjectLockConfiguration, PutObjectLockConfiguration | 1/10 | Governance/compliance. Not in current scope. |
| Bucket CORS | GetBucketCors, PutBucketCors, DeleteBucketCors | 1/10 | Relevant if abixio is accessed from browsers. |
| Bucket ACL | GetBucketAcl, PutBucketAcl, GetObjectAcl, PutObjectAcl | 1/10 | Legacy. AWS recommends policies over ACLs. |
| Bucket lifecycle | GetBucketLifecycleConfiguration, PutBucketLifecycleConfiguration, DeleteBucketLifecycle | 1/10 | Automatic object expiration/transition. |
| Bucket replication | GetBucketReplication, PutBucketReplication, DeleteBucketReplication | 1/10 | Cross-site replication. |
| Bucket notifications | GetBucketNotificationConfiguration, PutBucketNotificationConfiguration | 1/10 | Event notifications. |
| Bucket logging | GetBucketLogging, PutBucketLogging | 1/10 | Access logging. |
| Bucket website | GetBucketWebsite, PutBucketWebsite, DeleteBucketWebsite | 1/10 | Static hosting. |
| Analytics / metrics / inventory | All operations | 1/10 | AWS-specific. Not relevant. |
| S3 Select | SelectObjectContent | 1/10 | SQL queries on objects. Niche. |
| Presigned URLs | N/A (client-side) | N/A | Presigned URL generation is client-side. Server just needs to validate SigV4, which it does. |

## Response Field Coverage

How complete our responses are compared to what S3 clients expect.

### ListBuckets response

| Field | S3 spec | abixio returns | Gap |
|---|---|---|---|
| `Buckets.Bucket.Name` | bucket name | yes | -- |
| `Buckets.Bucket.CreationDate` | actual creation time | yes (filesystem mtime, ISO 8601) | -- |
| `Owner.ID` | account ID | hardcoded | cosmetic |
| `Owner.DisplayName` | account name | hardcoded | cosmetic |

### ListObjectsV2 response

| Field | S3 spec | abixio returns | Gap |
|---|---|---|---|
| `Name` | bucket name | yes | -- |
| `Prefix` | requested prefix | yes | -- |
| `KeyCount` | number of keys | yes | -- |
| `MaxKeys` | max keys requested | yes | -- |
| `IsTruncated` | pagination flag | yes | -- |
| `Contents.Key` | object key | yes | -- |
| `Contents.LastModified` | modification time | yes (from metadata) | -- |
| `Contents.ETag` | entity tag | yes | -- |
| `Contents.Size` | object size | yes | -- |
| `Contents.StorageClass` | storage class | hardcoded `STANDARD` | correct for single-tier |
| `CommonPrefixes.Prefix` | folder prefixes | yes | -- |
| `NextContinuationToken` | pagination token | yes | -- |
| `ContinuationToken` | echo of request token | not returned | minor gap |
| `Delimiter` | echo of delimiter | not returned | minor gap |
| `EncodingType` | encoding type | not returned | minor gap |
| `StartAfter` | not supported | not returned | not implemented |

### HeadObject / GetObject response headers

| Header | S3 spec | abixio returns | Gap |
|---|---|---|---|
| `Content-Type` | object content type | yes | -- |
| `Content-Length` | object size | yes | -- |
| `ETag` | entity tag | yes | -- |
| `Last-Modified` | modification time | yes (RFC 7231 HTTP-date) | -- |
| `Accept-Ranges` | `bytes` | yes | -- |
| `x-amz-meta-*` | custom metadata | yes | stored on PUT, returned on HEAD/GET |
| `x-amz-storage-class` | storage class | not returned | cosmetic |
| `x-amz-version-id` | version ID | not returned | no versioning |
| `x-amz-server-side-encryption` | encryption | not returned | no encryption |
| `Cache-Control` | cache control | not returned | not stored |
| `Content-Disposition` | disposition | not returned | not stored |
| `Content-Encoding` | encoding | not returned | not stored |

### Error responses

| Aspect | S3 spec | abixio | Gap |
|---|---|---|---|
| XML error body | `<Error><Code>...</Code><Message>...</Message></Error>` | yes | -- |
| Error codes | standard S3 error codes | partial (6 codes defined) | missing many codes |
| `RequestId` in errors | required | not returned | minor gap |
| `Resource` in errors | recommended | not returned | minor gap |

## Auth

| Aspect | S3 spec | abixio | Rating |
|---|---|---|---|
| SigV4 verification | required | yes (`src/s3/auth.rs`) | 8/10 |
| Anonymous access | optional | yes (configurable `no_auth`) | 8/10 |
| SigV4 chunked transfer | optional | not supported | 3/10 |
| Presigned URL validation | optional | not tested | unknown |

## Summary

### Overall S3 compliance: 7/10

abixio implements 17 of ~100 S3 API operations. The 17 it implements cover
the core object CRUD path including batch delete, server-side copy, range
requests, custom metadata, bucket lifecycle, and object/bucket tagging.
Response headers follow RFC 7231 (HTTP-date format). Everything else returns 405.

### Next priorities for server-side compliance

| Priority | What | Why |
|---|---|---|
| **Should** | Structured error responses | Return S3 error codes (AccessDenied, NoSuchBucket, etc.) with RequestId and Resource fields. |
| ~~**Later**~~ | ~~Object tagging~~ | Done. Object and bucket tagging implemented. |
| **Later** | Versioning | Version browser support. |
| **Later** | Multipart upload | Required for files >5GB. |
| **Later** | Bucket policies | Access control. |
