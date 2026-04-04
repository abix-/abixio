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
| ListBuckets | `GET /` | `list_buckets` | 7/10 | Works. Returns XML with bucket names. Creation dates are hardcoded to 2024-01-01. |
| CreateBucket | `PUT /{bucket}` | `create_bucket` | 7/10 | Works. No region, ACL, or object lock configuration support. |
| HeadBucket | `HEAD /{bucket}` | `head_bucket` | 8/10 | Works. Returns 200 or 404. |
| ListObjectsV2 | `GET /{bucket}` | `list_objects_handler` | 7/10 | Supports prefix, delimiter, max-keys. Has pagination (continuation token). Missing: list-type=2 query validation, encoding-type, start-after, fetch-owner. |
| PutObject | `PUT /{bucket}/{key}` | `put_object` | 7/10 | Reads full body, stores with content-type. Returns ETag. Missing: content-MD5 validation, metadata headers, storage class, tagging headers. |
| GetObject | `GET /{bucket}/{key}` | `get_object` | 7/10 | Returns body with Content-Type, Content-Length, ETag, Last-Modified. Missing: Range requests, If-Match/If-None-Match conditionals, response content overrides. |
| HeadObject | `HEAD /{bucket}/{key}` | `head_object` | 7/10 | Returns Content-Type, Content-Length, ETag, Last-Modified. Missing: metadata headers, storage class, version ID, encryption info. |
| DeleteObject | `DELETE /{bucket}/{key}` | `delete_object` | 8/10 | Returns 204 No Content. Missing: version ID support, MFA delete. |

## Not Implemented -- High Priority

These are S3 operations that clients commonly need and abixio does not route.
Any request to these endpoints currently returns 405 MethodNotAllowed.

| S3 API | HTTP | Rating | Impact |
|---|---|---|---|
| DeleteBucket | `DELETE /{bucket}` | 1/10 | Cannot delete buckets via S3 API. abixio-ui works around this by deleting all objects then calling delete, but the server has no DELETE bucket route. |
| DeleteObjects (batch) | `POST /{bucket}?delete` | 1/10 | Cannot batch-delete objects. Clients must delete one at a time. Blocks efficient recursive prefix delete. mc uses this for all `mc rm --recursive`. |
| CopyObject | `PUT /{bucket}/{key}` (x-amz-copy-source) | 1/10 | Cannot server-side copy. abixio-ui falls back to GET+PUT for cross-bucket, but same-bucket copy also fails because the server doesn't check the copy-source header. |
| DeleteBucket | `DELETE /{bucket}` | 1/10 | No route for bucket deletion. |

## Not Implemented -- Medium Priority

| S3 API | HTTP | Rating | Impact |
|---|---|---|---|
| GetObjectTagging | `GET /{bucket}/{key}?tagging` | 1/10 | No tag support. Tags drive lifecycle rules, billing, and access control. |
| PutObjectTagging | `PUT /{bucket}/{key}?tagging` | 1/10 | Same. |
| DeleteObjectTagging | `DELETE /{bucket}/{key}?tagging` | 1/10 | Same. |
| ListObjectVersions | `GET /{bucket}?versions` | 1/10 | No versioning support. Cannot list object versions. |
| GetBucketVersioning | `GET /{bucket}?versioning` | 1/10 | Cannot check if versioning is enabled. |
| PutBucketVersioning | `PUT /{bucket}?versioning` | 1/10 | Cannot enable/disable versioning. |
| GetBucketPolicy | `GET /{bucket}?policy` | 1/10 | No policy support. |
| PutBucketPolicy | `PUT /{bucket}?policy` | 1/10 | Same. |
| DeleteBucketPolicy | `DELETE /{bucket}?policy` | 1/10 | Same. |
| GetBucketEncryption | `GET /{bucket}?encryption` | 1/10 | No encryption config support. |
| PutBucketEncryption | `PUT /{bucket}?encryption` | 1/10 | Same. |
| GetBucketTagging | `GET /{bucket}?tagging` | 1/10 | No bucket tag support. |
| PutBucketTagging | `PUT /{bucket}?tagging` | 1/10 | Same. |
| DeleteBucketTagging | `DELETE /{bucket}?tagging` | 1/10 | Same. |

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
| `Buckets.Bucket.CreationDate` | actual creation time | hardcoded `2024-01-01T00:00:00.000Z` | should store and return real creation time |
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
| `Last-Modified` | modification time | yes | format is non-standard (ISO-ish, not HTTP-date) |
| `Accept-Ranges` | `bytes` | not returned | blocks range request detection |
| `x-amz-meta-*` | custom metadata | not returned | metadata not stored |
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

### Overall S3 compliance: 3/10

abixio implements 8 of ~100 S3 API operations. The 8 it implements cover
the core object CRUD path and work correctly. Everything else returns 405.

### Next priorities for server-side compliance

| Priority | What | Why |
|---|---|---|
| **Must** | DeleteBucket (`DELETE /{bucket}`) | Basic bucket lifecycle. Clients expect it. |
| **Must** | DeleteObjects (`POST /{bucket}?delete`) | Batch delete. Blocks efficient recursive delete. mc depends on it. |
| **Must** | CopyObject (`PUT /{bucket}/{key}` with x-amz-copy-source) | Server-side copy. Blocks move/rename without download+reupload. |
| **Should** | Custom metadata storage and return | Store x-amz-meta-* on put, return on head/get. |
| **Should** | Range requests (GetObject with Range header) | Required for large file partial downloads and resume. |
| **Should** | Last-Modified in HTTP-date format | Current format is non-standard. Some clients may not parse it. |
| **Should** | Real bucket creation dates | Currently hardcoded. |
| **Later** | Object tagging | Tags for lifecycle/billing/access. |
| **Later** | Versioning | Version browser support. |
| **Later** | Multipart upload | Required for files >5GB. |
| **Later** | Bucket policies | Access control. |
