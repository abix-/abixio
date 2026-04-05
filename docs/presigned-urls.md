# Presigned URL Authentication

How abixio validates presigned S3 URLs.

## What presigned URLs are

A presigned URL allows access to an S3 object without sending credentials in
the request headers. Instead, the signature is embedded in the URL query string.
The server validates the signature and checks expiration.

Example presigned URL:
```
http://localhost:9000/bucket/key?X-Amz-Algorithm=AWS4-HMAC-SHA256
  &X-Amz-Credential=mykey/20240101/us-east-1/s3/aws4_request
  &X-Amz-Date=20240101T000000Z
  &X-Amz-Expires=3600
  &X-Amz-SignedHeaders=host
  &X-Amz-Signature=abcdef1234567890...
```

## How validation works

When a request arrives, the auth handler checks in order:

1. **Presigned?** Check if query string contains `X-Amz-Algorithm` or `X-Amz-Credential`
2. **Header auth?** Check for `Authorization` header
3. **Anonymous?** If `no_auth` is enabled, allow without credentials

### Presigned validation steps

1. Parse 6 query parameters:
   - `X-Amz-Algorithm` -- must be `AWS4-HMAC-SHA256`
   - `X-Amz-Credential` -- `access_key/date/region/s3/aws4_request`
   - `X-Amz-Date` -- ISO 8601 format (`20240101T000000Z`)
   - `X-Amz-Expires` -- seconds until expiration (max 604800 = 7 days)
   - `X-Amz-SignedHeaders` -- semicolon-separated header names
   - `X-Amz-Signature` -- hex-encoded HMAC signature

2. Validate access key matches server config

3. Check clock skew: request date must not be more than 15 minutes in the future

4. Check expiration: `current_time - request_date > expires` means expired

5. Build canonical request:
   - HTTP method, canonical URI, canonical query string (all params except `X-Amz-Signature`, sorted)
   - Canonical headers from signed headers list
   - Payload hash: `UNSIGNED-PAYLOAD` (presigned URLs don't include body hash)

6. Compute signature using same HMAC chain as header auth:
   - String to sign = `AWS4-HMAC-SHA256\n<date>\n<scope>\n<canonical_request_hash>`
   - Signing key = HMAC chain of secret key + date + region + "s3" + "aws4_request"

7. Constant-time compare computed signature with provided `X-Amz-Signature`

## Implementation

- `src/s3/auth.rs`: `is_presigned()` detects presigned params, `verify_presigned_v4()` validates
- `src/s3/handlers.rs`: `check_auth()` tries presigned before header auth
- Reuses all existing SigV4 crypto: `derive_signing_key`, `canonical_uri`, `sha256_hex`, `hmac_sha256_hex`

## Client-side generation

Presigned URLs are generated entirely on the client side using `aws-sdk-s3`
presigning config. The server only validates -- it doesn't generate presigned
URLs.

## Limits

| Limit | Value |
|---|---|
| Max expiration | 604800 seconds (7 days) |
| Clock skew tolerance | 15 minutes |
| Algorithm | AWS4-HMAC-SHA256 only |
