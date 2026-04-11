# Conditional Requests

How abixio handles S3 conditional request headers on GET and HEAD operations.

> **Status: not yet implemented in s3s migration.** s3s parses these headers
> into the GetObjectInput/HeadObjectInput DTOs, but `s3_service.rs` does not
> evaluate them yet. This is a known gap tracked in docs/todo.md.

## Supported headers

| Header | Effect | HTTP Status |
|---|---|---|
| `If-None-Match` | Return 304 if ETag matches | 304 Not Modified |
| `If-Modified-Since` | Return 304 if not modified since date | 304 Not Modified |
| `If-Match` | Return 412 if ETag does NOT match | 412 Precondition Failed |
| `If-Unmodified-Since` | Return 412 if modified since date | 412 Precondition Failed |

## Evaluation order (per AWS spec)

1. **If-Match**: if present and ETag does NOT match, return 412.
2. **If-Unmodified-Since**: if present and object modified after date, return 412.
3. **If-None-Match**: if present and ETag matches, return 304.
4. **If-Modified-Since**: if present and object not modified after date, return 304.

## Implementation plan

s3s delivers these headers as fields on `GetObjectInput` and `HeadObjectInput`:
- `if_match: Option<IfMatch>`
- `if_modified_since: Option<IfModifiedSince>`
- `if_none_match: Option<IfNoneMatch>`
- `if_unmodified_since: Option<IfUnmodifiedSince>`

The `get_object` and `head_object` methods in `src/s3_service.rs` need to check
these fields after reading object metadata and before returning the body.

## Applies to

- `GET /{bucket}/{key}`: conditional GET
- `HEAD /{bucket}/{key}`: conditional HEAD

Does NOT apply to PUT, DELETE, or other operations.
