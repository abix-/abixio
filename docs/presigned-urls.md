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

SigV4 authentication (header auth, presigned URLs, and chunked transfer) is
handled entirely by the s3s protocol layer. abixio provides a credential
lookup via `impl S3Auth for AbixioAuth` in `src/s3_auth.rs`.

### Flow

1. s3s receives the HTTP request and detects the auth method (header, presigned, or chunked)
2. s3s calls `AbixioAuth::get_secret_key(access_key)` to look up the secret key
3. s3s validates the signature using the secret key
4. if valid, the request proceeds to the S3 operation handler
5. if invalid, s3s returns an appropriate error (AccessDenied, SignatureDoesNotMatch, etc.)

### What s3s handles

- presigned URL parameter parsing (Algorithm, Credential, Date, Expires, SignedHeaders, Signature)
- canonical request construction
- signing key derivation (HMAC chain)
- signature verification (constant-time compare)
- expiration checking
- clock skew validation
- chunked transfer signature verification
- trailing checksum verification

## Implementation

- `src/s3_auth.rs`: `AbixioAuth` implements `S3Auth::get_secret_key()` for credential lookup
- s3s handles all SigV4 crypto internally (no application code needed)

## Client-side generation

Presigned URLs are generated entirely on the client side using `aws-sdk-s3`
presigning config. The server only validates -- it doesn't generate presigned
URLs.

## Limits

| Limit | Value |
|---|---|
| Max expiration | 604800 seconds (7 days, enforced by s3s) |
| Clock skew tolerance | per s3s defaults |
| Algorithm | AWS4-HMAC-SHA256 (SigV4) |
