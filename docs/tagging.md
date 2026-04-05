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
version entry is modified in `meta.json` across all disks.

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

Bucket-level tags are stored in `.tagging.json` in the bucket directory
on the first disk. Uses the same S3 XML format as object tagging.

| Endpoint | Description |
|---|---|
| `GET /{bucket}?tagging` | Get bucket tags |
| `PUT /{bucket}?tagging` | Set bucket tags |
| `DELETE /{bucket}?tagging` | Remove bucket tags |

Bucket tags are stored as a simple JSON key-value map:

```json
{ "project": "alpha", "cost-center": "12345" }
```
