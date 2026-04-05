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

- Body must be valid JSON
- `Version` field must exist and be non-empty
- Max body size: 20KiB

### Error cases

| Case | Response |
|---|---|
| No policy set | 404 `NoSuchBucketPolicy` |
| Invalid JSON | 400 `MalformedXML` |
| Missing/empty Version | 400 `MalformedXML` |
| Body > 20KiB | 400 `PolicyTooLarge` |
| Bucket not found | 404 `NoSuchBucket` |

## Storage

Policy stored as `.policy.json` in the bucket directory on the first disk.
Same pattern as `.versioning.json` and `.tagging.json`.

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

- `mc anonymous set-json /path/to/policy.json ALIAS/BUCKET` -- stores raw policy
- `mc anonymous get-json ALIAS/BUCKET` -- retrieves stored policy
- `mc anonymous set download/public/upload/private ALIAS/BUCKET` -- canned policies
- `aws-sdk-s3`: `get_bucket_policy()`, `put_bucket_policy()`, `delete_bucket_policy()`
