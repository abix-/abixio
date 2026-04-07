# Error Responses

How abixio returns S3-compatible error responses.

## Format

s3s generates all error XML from smithy models (AWS spec-compliant).

Every error response includes:
- HTTP status code
- XML body with error details
- `x-amz-request-id` header (UUID, set by AbixioDispatch)

```xml
<Error>
  <Code>NoSuchKey</Code>
  <Message>The specified key does not exist</Message>
  <RequestId>550e8400-e29b-41d4-a716-446655440000</RequestId>
</Error>
```

## Request ID

Generated per request as a UUID v4 string. Set by the dispatch layer
(`src/s3_route.rs`) on every response (success and error) as the
`x-amz-request-id` header. s3s includes it in error XML bodies.

## Error codes

s3s provides all standard S3 error codes via the smithy-generated
`S3ErrorCode` enum. abixio uses a subset:

| Code | HTTP | When |
|---|---|---|
| `NoSuchBucket` | 404 | Bucket does not exist |
| `NoSuchKey` | 404 | Object does not exist |
| `BucketAlreadyOwnedByYou` | 409 | CreateBucket on existing bucket |
| `BucketNotEmpty` | 409 | DeleteBucket on non-empty bucket |
| `IncompleteBody` | 400 | Request body could not be read |
| `InvalidArgument` | 400 | Invalid config, bad object key, bad version ID |
| `InvalidBucketName` | 400 | Bucket name validation failure |
| `InternalError` | 500 | Write/read quorum failure, bitrot, IO errors |
| `ServiceUnavailable` | 503 | Node is fenced or cluster is not ready |
| `NoSuchUpload` | 404 | Invalid or expired multipart upload ID |
| `NoSuchBucketPolicy` | 404 | Bucket policy is missing |
| `NoSuchLifecycleConfiguration` | 404 | Lifecycle configuration is missing |
| `NoSuchCORSConfiguration` | 404 | CORS configuration is missing |
| `NotImplemented` | 501 | Unimplemented S3 operation (CORS PUT, etc.) |
| `InvalidAccessKeyId` | 403 | Unknown access key in auth |

Any unimplemented S3 operation returns `NotImplemented` via s3s's default
trait method.

## Error mapping

Storage errors map to S3 error codes via `map_err()` in `src/s3_service.rs`:

| StorageError | S3 Code |
|---|---|
| `BucketNotFound` | NoSuchBucket |
| `ObjectNotFound` | NoSuchKey |
| `BucketExists` | BucketAlreadyOwnedByYou |
| `BucketNotEmpty` | BucketNotEmpty |
| `WriteQuorum` | InternalError |
| `ReadQuorum` | InternalError |
| `Bitrot` | InternalError |
| `InvalidConfig(_)` | InvalidArgument |
| `Io` | InternalError |
| `InvalidBucketName(_)` | InvalidBucketName |
| `InvalidObjectKey(_)` | InvalidArgument |
| `InvalidVersionId(_)` | InvalidArgument |
| `InvalidUploadId(_)` | NoSuchUpload |
| `Internal(_)` | InternalError |
