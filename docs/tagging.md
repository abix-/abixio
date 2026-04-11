# Object and Bucket Tagging

How abixio implements S3 object and bucket tagging.

## Object tagging

Tags are key-value string pairs stored in `meta.json` as part of each version's
metadata. S3 spec allows up to 10 tags per object, keys up to 128 chars,
values up to 256 chars.

### S3 endpoints

| Endpoint | Description |
|---|---|
| `GET /{bucket}/{key}?tagging` | Get object tags |
| `PUT /{bucket}/{key}?tagging` | Set object tags (replaces all) |
| `DELETE /{bucket}/{key}?tagging` | Remove all object tags |

### Storage

Tags live in the `tags` field of each version entry in `meta.json`:

```json
{
  "versions": [{
    "tags": {
      "env": "production",
      "team": "infra"
    },
    ...
  }]
}
```

When tags are updated via `PUT ?tagging`, the `tags` field on the latest
non-delete-marker version entry is modified in `meta.json` across all disks.

### Request/response format

PUT body and GET response use S3 standard XML:

```xml
<Tagging>
  <TagSet>
    <Tag>
      <Key>env</Key>
      <Value>production</Value>
    </Tag>
    <Tag>
      <Key>team</Key>
      <Value>infra</Value>
    </Tag>
  </TagSet>
</Tagging>
```

DELETE returns 204 No Content.

### Error cases

| Case | Response |
|---|---|
| Object does not exist | 404 NoSuchKey |
| Malformed XML body | 400 MalformedXML |
| Bucket does not exist | 404 NoSuchBucket |

## Bucket tagging

Bucket-level tags are stored in `.abixio.sys/buckets/<bucket>/settings.json`
under the `tags` field. Uses the same S3 XML format as object tagging.

| Endpoint | Description |
|---|---|
| `GET /{bucket}?tagging` | Get bucket tags |
| `PUT /{bucket}?tagging` | Set bucket tags |
| `DELETE /{bucket}?tagging` | Remove bucket tags |

Bucket tags are stored as a simple JSON key-value map:

```json
{ "project": "alpha", "cost-center": "12345" }
```

Current implementation note:

- bucket tag writes verify bucket existence
- bucket tag reads currently return an empty tag set if the tag file is missing
  instead of returning a dedicated S3 tagging error

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| Object tagging GET/PUT/DELETE endpoints are implemented | Verified | `src/s3_service.rs:775-825`, `tests/s3_integration.rs:568-675` |
| Bucket tagging GET/PUT/DELETE endpoints are implemented | Verified | `src/s3_service.rs:828-882`, `tests/s3_integration.rs:678-708` |
| Object tags are stored in each version entry's `tags` field in `meta.json` | Verified | `src/storage/metadata.rs`, object-tagging storage methods in the storage layer, exercised by `tests/s3_integration.rs` |
| Updating object tags modifies the latest non-delete-marker version metadata | Plausible and consistent with behavior, not re-read in full storage implementation here | Endpoint behavior is tested, but I did not walk every storage helper in this pass |
| Bucket tags live in bucket settings and missing bucket tags return an empty tag set | Verified | `src/s3_service.rs:828-882` |
| S3 tag-count and key/value-length limits are enforced | Not verified in current code | This page should not claim enforcement unless explicit validation is added; current endpoint code mainly transforms XML to `HashMap` and persists it |

Verdict: tagging support is present and tested. The main remaining risk is undocumented or unenforced AWS tag-limit behavior rather than missing core functionality.
