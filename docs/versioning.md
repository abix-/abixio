# Versioning

How abixio implements S3 object versioning. The overall model follows AWS S3
and MinIO, with a few implementation simplifications called out below.

## Bucket versioning states

Each bucket has one of three versioning states:

| State | Behavior | Config file |
|---|---|---|
| **Disabled** (default) | PUT overwrites, DELETE removes. No version IDs. | No `versioning` field in settings.json |
| **Enabled** | PUT creates new UUID version. DELETE adds delete marker. Old versions preserved. | `"versioning": "Enabled"` |
| **Suspended** | PUT overwrites the current null-version object and returns `x-amz-version-id: null`. DELETE adds delete marker. Existing versioned history is preserved. | `"versioning": "Suspended"` |

Config stored in `.abixio.sys/buckets/<bucket>/settings.json` on each disk.

## S3 endpoints

| Endpoint | Description |
|---|---|
| `PUT /{bucket}?versioning` | Set versioning status (Enabled/Suspended) |
| `GET /{bucket}?versioning` | Get current versioning status |
| `GET /{bucket}?versions` | List all versions of all objects |
| `GET /{bucket}/{key}?versionId=X` | Get specific version |
| `DELETE /{bucket}/{key}?versionId=X` | Permanently delete specific version |
| `DELETE /{bucket}/{key}` (versioned) | Add delete marker (data preserved) |

## How PUT works

### Versioning disabled or suspended
```
1. Erasure-encode data into shards
2. Write shard to key/shard.dat on each disk
3. Write meta.json with single version entry (version_id = "")
```
Each PUT replaces the previous object entirely.

When versioning is suspended, the response header still returns
`x-amz-version-id: null` even though the stored object uses the unversioned
layout on disk.

### Versioning enabled
```
1. Generate UUID version ID
2. Erasure-encode data into shards
3. Write shard to key/<uuid>/shard.dat on each disk
4. Update meta.json: prepend new version entry, mark previous as not latest
```
Old versions and their shard data are preserved.

## How GET works

1. Read `meta.json` from the object directory
2. Find the first entry in `versions` where `is_delete_marker == false`
3. Determine shard path:
   - If `version_id` is empty: `key/shard.dat` (unversioned)
   - If `version_id` is a UUID: `key/<uuid>/shard.dat` (versioned)
4. Read shards from all disks, erasure-decode to reconstruct original data

With `?versionId=X`: find the matching entry in `versions`, read from that
version's shard path.

## How DELETE works

### Versioning disabled
Object directory is removed entirely (all shards + meta.json).

### Versioning enabled or suspended (no versionId)
A delete marker is added to `meta.json`:
```json
{
  "version_id": "new-uuid",
  "is_latest": true,
  "is_delete_marker": true,
  "size": 0,
  "etag": ""
}
```
No shard data is created. Subsequent GETs return 404 because the latest
version is a delete marker.

Current implementation note:

- list versions correctly shows the delete marker
- plain `GET /bucket/key` and `HEAD /bucket/key` currently still resolve the
  latest non-delete-marker version rather than returning a delete-marker-style
  404

### DELETE with versionId
The specific version entry is removed from `meta.json` and its shard data
directory (`key/<uuid>/`) is deleted. This is a permanent delete.

## Response headers

| Header | When | Status |
|---|---|---|
| `x-amz-version-id` | On versioned PUT, suspended PUT (`null`), versioned DELETE, and version-addressed GET/HEAD responses | Implemented through s3s DTOs |
| `x-amz-delete-marker: true` | On DELETE when a delete marker is created | Implemented through s3s DTOs |

> **Current gap:** when the latest version is a delete marker, plain
> `GET /bucket/key` and `HEAD /bucket/key` still resolve the latest
> non-delete-marker version instead of surfacing delete-marker semantics.
> Version-addressed reads and `ListObjectVersions` behave correctly.

## ListObjectVersions response

Returns XML `<ListVersionsResult>` containing `<Version>` and `<DeleteMarker>`
elements for each version of each object matching the prefix.

```xml
<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bucket</Name>
  <Prefix/>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Version>
    <Key>mykey</Key>
    <VersionId>uuid-2</VersionId>
    <IsLatest>true</IsLatest>
    <LastModified>2024-01-01T00:00:00.000Z</LastModified>
    <ETag>"abc123"</ETag>
    <Size>1024</Size>
    <StorageClass>STANDARD</StorageClass>
  </Version>
  <Version>
    <Key>mykey</Key>
    <VersionId>uuid-1</VersionId>
    <IsLatest>false</IsLatest>
    ...
  </Version>
  <DeleteMarker>
    <Key>mykey</Key>
    <VersionId>uuid-3</VersionId>
    <IsLatest>true</IsLatest>
    ...
  </DeleteMarker>
</ListVersionsResult>
```

## Interoperability

Tested with:
- `mc version enable/suspend/info`: MinIO client versioning commands
- `mc ls --versions`: list object versions
- `mc rm --version-id=X`: delete specific version
- `aws-sdk-s3` (Rust): used by abixio-ui

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| Bucket versioning states `Disabled`, `Enabled`, `Suspended` | Verified | `src/s3_service.rs:903-943`, `tests/s3_integration.rs:873-977` |
| Suspended versioning returns `x-amz-version-id: null` on PUT | Verified | `src/s3_service.rs:416-420`, `tests/s3_integration.rs:1248-1279` |
| Enabled versioning returns UUID version IDs on PUT | Verified | `src/s3_service.rs:380-423`, `tests/s3_integration.rs:981-1027` |
| `GET /{bucket}?versions` lists versions and delete markers | Verified | `src/s3_service.rs:946-980`, `tests/s3_integration.rs:1032-1136` |
| `GET /{bucket}/{key}?versionId=X` reads a specific version | Verified | `src/s3_service.rs:433-435`, storage `get_object_version` in `src/storage/volume_pool.rs:900-919`, test `tests/s3_integration.rs:1141-1186` |
| `DELETE /{bucket}/{key}?versionId=X` permanently deletes a specific version | Verified | `src/s3_service.rs:540-549`, storage `delete_object_version` in `src/storage/volume_pool.rs:922-937`, test `tests/s3_integration.rs:1189-1243` |
| Response headers were not wired through s3s DTOs | Corrected | DTO fields are set in `src/s3_service.rs:416-420`, `489-491`, `529-530`, `546-547`, `571-574`; tests assert headers in `tests/s3_integration.rs:998-1027`, `1124-1126`, `1229`, `1279` |
| Plain GET after delete marker still returns the latest live version | Verified | Storage read path finds latest non-delete-marker in `src/storage/local_volume.rs:666-670`, `709-723`, `773-787`; delete-marker behavior test exists for listing, not for plain GET |
| On-disk config location `.abixio.sys/buckets/<bucket>/settings.json` | Plausible but not re-opened in this pass | This doc’s higher-level path claim was not directly re-traced in source during this audit |

Verdict: the core versioning behavior is implemented and tested. The document was stale mainly in its response-header section. The remaining important behavioral gap is delete-marker semantics on plain GET/HEAD.
