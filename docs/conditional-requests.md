# Conditional Requests

How abixio handles S3 conditional request headers on GET and HEAD operations.

> **Status: implemented for GET and HEAD.** `s3_service.rs` evaluates the
> conditional headers after loading object metadata and before returning the
> response body or HEAD metadata.

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

That check is now implemented via `check_conditionals()` in
`src/s3_service.rs`.

## Applies to

- `GET /{bucket}/{key}`: conditional GET
- `HEAD /{bucket}/{key}`: conditional HEAD

Does NOT apply to PUT, DELETE, or other operations.

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| Conditional GET/HEAD is not yet implemented | Corrected | `check_conditionals()` is called from both `get_object()` and `head_object()` in `src/s3_service.rs:444-451`, `505-512` |
| Supported headers are `If-Match`, `If-None-Match`, `If-Modified-Since`, and `If-Unmodified-Since` | Verified | `src/s3_service.rs:242-300` |
| Conditional checks run after object metadata is loaded and before the response is returned | Verified | `src/s3_service.rs` GET/HEAD paths |
| These conditionals apply to GET and HEAD, not PUT/DELETE | Verified in current implementation | Only GET/HEAD call `check_conditionals()` |
| Behavior is covered by tests | Verified | `tests/s3_integration.rs:732-815` |

Verdict: this page had gone stale. Conditional GET and HEAD are implemented now; the remaining work is documentation cleanup, not code support.
