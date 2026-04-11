# Bucket Policy

How abixio stores and serves S3 bucket policies.

## S3 endpoints

| Endpoint | HTTP | Description |
|---|---|---|
| `PUT /{bucket}?policy` | PUT | Store a bucket policy (JSON body, max 20KiB) |
| `GET /{bucket}?policy` | GET | Return stored policy as JSON |
| `DELETE /{bucket}?policy` | DELETE | Remove bucket policy, returns 204 (currently idempotent) |

## Policy format

Standard AWS IAM bucket policy JSON:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "PublicRead",
      "Effect": "Allow",
      "Principal": {"AWS": ["*"]},
      "Action": ["s3:GetObject"],
      "Resource": ["arn:aws:s3:::mybucket/*"]
    }
  ]
}
```

### Validation on PUT

The s3s protocol layer delivers the raw policy body as a string. The current
implementation validates that the body is valid JSON and that the top-level
`Version` field exists and is non-empty before storing it.

### Error cases

| Case | Response |
|---|---|
| No policy set | 404 `NoSuchBucketPolicy` |
| Bucket not found | 404 `NoSuchBucket` |

## Storage

Policy stored in `.abixio.sys/buckets/<bucket>/settings.json` under the
`policy` field. Same file as versioning, EC, tags, and lifecycle config.

Current implementation note:

- `PUT` and `GET` verify bucket existence
- `DELETE` currently removes the file idempotently and returns `204` even if the
  bucket does not exist

## Policy enforcement

**Not implemented.** Policies are stored and retrieved but not evaluated
against requests. All access control is currently handled by SigV4 auth
(authenticated = full access, `--no-auth` = open). Policy enforcement
would require evaluating each request against the stored policy statements
at request time.

## Interoperability

- `mc anonymous set-json /path/to/policy.json ALIAS/BUCKET`: stores raw policy
- `mc anonymous get-json ALIAS/BUCKET`: retrieves stored policy
- `mc anonymous set download/public/upload/private ALIAS/BUCKET`: canned policies
- `aws-sdk-s3`: `get_bucket_policy()`, `put_bucket_policy()`, `delete_bucket_policy()`

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| `GET`, `PUT`, and `DELETE` bucket policy endpoints are implemented | Verified | `src/s3_service.rs:1177-1241`, `tests/s3_integration.rs:2723-2838` |
| PUT stores the raw policy body without validation | Corrected | Current code parses JSON and requires a non-empty `Version` field, returning `MalformedPolicy` on failure (`src/s3_service.rs:1206-1213`) |
| Policy is stored in bucket settings under the `policy` field | Verified | `src/storage/metadata.rs`, `src/s3_service.rs:1214-1221` |
| GET without a stored policy returns `NoSuchBucketPolicy` | Verified | `src/s3_service.rs:1186-1194`, `tests/s3_integration.rs:2761-2770` |
| DELETE is idempotent and currently writes `policy = null` | Verified | `src/s3_service.rs:1227-1241`, `tests/s3_integration.rs:2822-2838` |
| Policy enforcement is not implemented | Verified | No request-time policy evaluation path exists in `src/s3_service.rs` or dispatch/auth code |

Verdict: bucket-policy storage and retrieval are implemented and tested. The main stale point was PUT validation: the server no longer stores arbitrary raw strings without checking JSON and `Version`.
