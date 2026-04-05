# Error Responses

How abixio returns S3-compatible error responses.

## Format

Every error response includes:
- HTTP status code
- XML body with error details
- `x-amz-request-id` header (hex-encoded nanosecond timestamp)

```xml
<Error>
  <Code>NoSuchKey</Code>
  <Message>The specified key does not exist</Message>
  <RequestId>18A353E4465EFBFC</RequestId>
  <Resource>/bucket/key</Resource>
</Error>
```

## Request ID

Generated per request as `format!("{:X}", SystemTime::now().as_nanos())`.
Set on every response (success and error) as the `x-amz-request-id` header.
Also included in error XML bodies.

Matches MinIO's approach: `fmt.Sprintf("%X", t.UnixNano())`.

## Error codes

| Code | HTTP | When |
|---|---|---|
| `NoSuchBucket` | 404 | Bucket does not exist |
| `NoSuchKey` | 404 | Object does not exist |
| `BucketAlreadyOwnedByYou` | 409 | CreateBucket on existing bucket |
| `BucketNotEmpty` | 409 | DeleteBucket on non-empty bucket |
| `IncompleteBody` | 400 | Request body could not be read |
| `MalformedXML` | 400 | XML body could not be parsed |
| `InternalError` | 500 | Write/read quorum failure, bitrot, IO errors |
| `MethodNotAllowed` | 405 | Unsupported HTTP method |
| `InvalidRange` | 416 | Range header not satisfiable |
| `ServiceUnavailable` | 503 | Node is fenced or cluster is not ready to serve |
| `AccessDenied` | 403 | Auth failure or expired presigned URL |
| `PreconditionFailed` | 412 | If-Match or If-Unmodified-Since failed |
| `InvalidRequest` | 400 | Invalid per-object FTT (ftt >= disk count) |
| `NoSuchBucketPolicy` | 404 | Bucket policy is missing |
| `PolicyTooLarge` | 400 | Bucket policy exceeds the maximum document size |
| `NoSuchLifecycleConfiguration` | 404 | Lifecycle configuration is missing |
| `NoSuchCORSConfiguration` | 404 | CORS configuration is missing |
| `NotImplemented` | 501 | Stubbed feature path such as CORS or notification PUT |

## Error mapping

Storage errors map to S3 error codes via `map_error()` in `src/s3/errors.rs`:

| StorageError | S3 Code |
|---|---|
| `BucketNotFound` | NoSuchBucket |
| `ObjectNotFound` | NoSuchKey |
| `BucketExists` | BucketAlreadyOwnedByYou |
| `BucketNotEmpty` | BucketNotEmpty |
| `WriteQuorum` | InternalError |
| `ReadQuorum` | InternalError |
| `Bitrot` | InternalError |
| `InvalidConfig(_)` | InternalError |
| `Io` | InternalError |

Important distinction:

- `InvalidRequest` is used directly by request handlers for bad client input,
  such as invalid per-object EC headers
- `map_error()` currently does **not** convert `StorageError::InvalidConfig(_)`
  into `InvalidRequest`; it maps it to `InternalError`
