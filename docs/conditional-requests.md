# Conditional Requests

How abixio handles S3 conditional request headers on GET and HEAD operations.

## Supported headers

| Header | Effect | HTTP Status |
|---|---|---|
| `If-None-Match` | Return 304 if ETag matches | 304 Not Modified |
| `If-Modified-Since` | Return 304 if not modified since date | 304 Not Modified |
| `If-Match` | Return 412 if ETag does NOT match | 412 Precondition Failed |
| `If-Unmodified-Since` | Return 412 if modified since date | 412 Precondition Failed |

## Evaluation order

Current implementation order in `check_preconditions()`:

1. **If-None-Match** -- checked first. If the object's ETag matches the value
   (with or without surrounding quotes), return 304. Takes precedence over
   If-Modified-Since.

2. **If-Modified-Since** -- if the object's last-modified time is not after
   the given date, return 304.

3. **If-Match** -- if the object's ETag does NOT match the value, return 412.
   The wildcard `*` matches any ETag.

4. **If-Unmodified-Since** -- if the object's last-modified time is after the
   given date, return 412.

## Date format

`If-Modified-Since` and `If-Unmodified-Since` expect RFC 7231 HTTP-date format:
```
Thu, 01 Jan 2024 00:00:00 GMT
```

This is the same format returned in the `Last-Modified` response header.

## ETag comparison

ETags are compared with quote stripping. All of these match:
- `"abc123"` matches `abc123`
- `"abc123"` matches `"abc123"`
- `*` matches any ETag for both `If-Match` and `If-None-Match`

## Applies to

- `GET /{bucket}/{key}` -- conditional GET
- `HEAD /{bucket}/{key}` -- conditional HEAD

Does NOT apply to PUT, DELETE, or other operations.

## Implementation

`check_preconditions()` in `src/s3/handlers.rs` takes the request headers,
object ETag, and last-modified timestamp. Returns `Some(Response)` if a
precondition triggers, `None` if the request should proceed normally.

Called from both `get_object` and `head_object` handlers after reading object
metadata.

## Use cases

- **Browser caching**: `If-None-Match` with cached ETag avoids re-downloading
  unchanged files.
- **Optimistic concurrency**: `If-Match` ensures you're updating the version
  you expect.
- **CDN cache validation**: `If-Modified-Since` checks if content has changed.
