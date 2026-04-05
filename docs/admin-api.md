# Admin API

AbixIO exposes a JSON admin API under the `/_admin/` path prefix. These
endpoints are not part of the S3 spec -- they provide server health,
disk status, object inspection, and heal controls.

All responses are `Content-Type: application/json`. Errors return a JSON
body with an `error` field.

## Endpoints

| Endpoint | Method | Description |
|---|---|---|
| `/_admin/status` | GET | Server config and uptime |
| `/_admin/disks` | GET | Per-disk health, space, and object counts |
| `/_admin/heal` | GET | MRF queue and scanner statistics |
| `/_admin/heal?bucket=X&key=Y` | POST | Trigger manual heal for one object |
| `/_admin/object?bucket=X&key=Y` | GET | Shard-level object inspection |
| `/_admin/bucket/{name}/ec` | GET | Get bucket EC config (or server default) |
| `/_admin/bucket/{name}/ec?data=N&parity=N` | PUT | Set bucket EC default |

## GET /_admin/status

Returns server configuration and uptime.

```bash
curl http://localhost:10000/_admin/status
```

```json
{
  "server": "abixio",
  "version": "0.1.0",
  "uptime_secs": 3600,
  "data_shards": 2,
  "parity_shards": 2,
  "total_disks": 4,
  "listen": ":10000",
  "auth_enabled": true,
  "scan_interval": "10m",
  "heal_interval": "24h",
  "mrf_workers": 2
}
```

| Field | Type | Description |
|---|---|---|
| `server` | string | Always `"abixio"` |
| `version` | string | Cargo package version |
| `uptime_secs` | u64 | Seconds since server start |
| `data_shards` | usize | Server default data shard count |
| `parity_shards` | usize | Server default parity shard count |
| `total_disks` | usize | Number of disks in pool |
| `listen` | string | Listen address from config |
| `auth_enabled` | bool | True unless `--no-auth` was passed |
| `scan_interval` | string | Scanner loop interval (e.g. `"10m"`) |
| `heal_interval` | string | Per-object recheck cooldown (e.g. `"24h"`) |
| `mrf_workers` | usize | Number of MRF heal workers |

## GET /_admin/disks

Returns per-disk health, space usage, and object counts.

```bash
curl http://localhost:10000/_admin/disks
```

```json
{
  "disks": [
    {
      "index": 0,
      "path": "/data/d0",
      "online": true,
      "total_bytes": 107374182400,
      "used_bytes": 10737418240,
      "free_bytes": 96636764160,
      "bucket_count": 3,
      "object_count": 150
    }
  ]
}
```

| Field | Type | Description |
|---|---|---|
| `index` | usize | Disk index in pool (0-based) |
| `path` | string | Disk path or backend label |
| `online` | bool | True if disk responded to space query |
| `total_bytes` | u64 | Total filesystem capacity (0 if offline) |
| `used_bytes` | u64 | Used bytes (0 if offline) |
| `free_bytes` | u64 | Free bytes (0 if offline) |
| `bucket_count` | usize | Number of buckets on this disk |
| `object_count` | usize | Total objects across all buckets on this disk |

## GET /_admin/heal

Returns MRF queue status and scanner statistics.

```bash
curl http://localhost:10000/_admin/heal
```

```json
{
  "mrf_pending": 0,
  "mrf_workers": 2,
  "scanner": {
    "running": true,
    "scan_interval": "10m",
    "heal_interval": "24h",
    "objects_scanned": 1500,
    "objects_healed": 3,
    "last_scan_started": 120,
    "last_scan_duration_secs": 45
  }
}
```

| Field | Type | Description |
|---|---|---|
| `mrf_pending` | usize | Items waiting in MRF heal queue |
| `mrf_workers` | usize | Number of MRF drain workers |
| `scanner.running` | bool | Always true while server is up |
| `scanner.scan_interval` | string | How often the scanner runs |
| `scanner.heal_interval` | string | Per-object recheck cooldown |
| `scanner.objects_scanned` | u64 | Total objects checked since server start |
| `scanner.objects_healed` | u64 | Total objects healed since server start |
| `scanner.last_scan_started` | u64 | Seconds since last scan started (0 if never) |
| `scanner.last_scan_duration_secs` | u64 | Duration of last completed scan in seconds |

## POST /_admin/heal?bucket=X&key=Y

Trigger an immediate heal for a specific object.

```bash
curl -X POST "http://localhost:10000/_admin/heal?bucket=mybucket&key=photo.jpg"
```

### Possible results

**Healthy** (no repair needed):
```json
{
  "result": "healthy"
}
```

**Repaired** (shards reconstructed):
```json
{
  "result": "repaired",
  "shards_fixed": 2
}
```

**Unrecoverable** (too many shards lost):
```json
{
  "result": "unrecoverable",
  "error": "not enough healthy shards"
}
```

| Field | Type | Description |
|---|---|---|
| `result` | string | `"healthy"`, `"repaired"`, or `"unrecoverable"` |
| `shards_fixed` | usize? | Number of shards reconstructed (only on `"repaired"`) |
| `error` | string? | Error detail (only on `"unrecoverable"`) |

### Error cases

| Case | HTTP | Response |
|---|---|---|
| Missing `bucket` param | 400 | `{"error": "missing bucket parameter"}` |
| Missing `key` param | 400 | `{"error": "missing key parameter"}` |
| Internal failure | 500 | `{"error": "<detail>"}` |

## GET /_admin/object?bucket=X&key=Y

Inspect an object's shard status across all disks. Shows per-shard health,
checksums, and erasure distribution.

```bash
curl "http://localhost:10000/_admin/object?bucket=mybucket&key=photo.jpg"
```

```json
{
  "bucket": "mybucket",
  "key": "photo.jpg",
  "size": 1024,
  "etag": "d41d8cd98f00b204e9800998ecf8427e",
  "content_type": "image/jpeg",
  "created_at": 1700000000,
  "erasure": {
    "data": 2,
    "parity": 2,
    "distribution": [2, 0, 3, 1]
  },
  "shards": [
    { "index": 0, "disk": 2, "status": "ok", "checksum": "abc123..." },
    { "index": 1, "disk": 0, "status": "ok", "checksum": "def456..." },
    { "index": 2, "disk": 3, "status": "missing", "checksum": null },
    { "index": 3, "disk": 1, "status": "ok", "checksum": "ghi789..." }
  ]
}
```

### Shard status values

| Status | Meaning |
|---|---|
| `ok` | Metadata matches consensus and SHA-256 checksum verified |
| `corrupt` | Metadata or checksum mismatch (bitrot or stale data) |
| `missing` | Shard not found on expected disk |

### Error cases

| Case | HTTP | Response |
|---|---|---|
| Missing `bucket` param | 400 | `{"error": "missing bucket parameter"}` |
| Missing `key` param | 400 | `{"error": "missing key parameter"}` |
| Object not on any disk | 404 | `{"error": "object not found on any disk"}` |

## GET /_admin/bucket/{name}/ec

Get the EC configuration for a bucket. Returns the bucket-level override
if set, otherwise returns the server default with `"source": "server_default"`.

```bash
curl http://localhost:10000/_admin/bucket/mybucket/ec
```

**Bucket override set:**
```json
{
  "data": 3,
  "parity": 3
}
```

**No override (server default):**
```json
{
  "data": 2,
  "parity": 2,
  "source": "server_default"
}
```

## PUT /_admin/bucket/{name}/ec?data=N&parity=N

Set a default EC ratio for new objects in a bucket. Stored as `.ec.json`
on each disk. Does not affect existing objects.

```bash
curl -X PUT "http://localhost:10000/_admin/bucket/mybucket/ec?data=3&parity=3"
```

```json
{
  "data": 3,
  "parity": 3
}
```

### Error cases

| Case | HTTP | Response |
|---|---|---|
| Missing `data` param | 400 | `{"error": "missing data parameter"}` |
| Missing `parity` param | 400 | `{"error": "missing parity parameter"}` |
| Invalid config | 400 | `{"error": "<detail>"}` |

See [per-object-ec.md](per-object-ec.md) for the full EC precedence chain
(per-object header > bucket config > server default).

## Implementation

- `src/admin/handlers.rs` -- `AdminHandler` with dispatch and all endpoint methods
- `src/admin/types.rs` -- JSON response structs (serde `Serialize`)
- `src/admin/mod.rs` -- `HealStats` shared state (atomic counters + uptime)
- `tests/admin_integration.rs` -- 19 integration tests
- `tests/e2e.py` -- end-to-end Python test covering admin endpoints
