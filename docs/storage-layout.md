# Storage Layout

How abixio stores data on disk. Every object uses the same layout regardless
of whether versioning is enabled.

## Disk structure

abixio uses erasure coding across multiple disks. With `--data 2 --parity 2`,
each object is split into 4 shards distributed across 4 disks.

```
disk0/
  bucket-name/
    .versioning.json          # optional: { "status": "Enabled" | "Suspended" }
    .tagging.json             # optional: bucket-level tags
    .ec.json                  # optional: { "data": N, "parity": N } bucket EC default
    object-key/
      meta.json               # all version metadata for this object
      shard.dat               # shard data (unversioned objects)
      <uuid>/                 # shard data (versioned objects, one dir per version)
        shard.dat

disk1/
  bucket-name/
    object-key/
      meta.json               # same structure, different shard index/checksum
      shard.dat
```

Each disk holds one shard per object. The shard index and checksum differ
per disk, but the rest of the metadata is identical.

## meta.json

Single source of truth for all version metadata. Contains a `versions` array
ordered newest-first.

```json
{
  "versions": [
    {
      "size": 1024,
      "etag": "d41d8cd98f00b204e9800998ecf8427e",
      "content_type": "text/plain",
      "created_at": 1700000000,
      "erasure": {
        "data": 2,
        "parity": 2,
        "index": 0,
        "distribution": [2, 0, 3, 1],
        "epoch_id": 7,
        "set_id": "cluster-set-4x2",
        "node_ids": ["node-3", "node-1", "node-4", "node-2"],
        "disk_ids": ["node-3-disk-1", "node-1-disk-1", "node-4-disk-1", "node-2-disk-1"]
      },
      "checksum": "abc123def456...",
      "user_metadata": { "x-amz-meta-author": "alice" },
      "tags": { "env": "prod" },
      "version_id": "550e8400-e29b-41d4-a716-446655440000",
      "is_latest": true,
      "is_delete_marker": false
    }
  ]
}
```

### Fields

| Field | Description |
|---|---|
| `size` | Original object size in bytes (before erasure splitting) |
| `etag` | MD5 hex of the original object data |
| `content_type` | MIME type set on upload |
| `created_at` | Unix timestamp (seconds) of upload time |
| `erasure.data` | Number of data shards |
| `erasure.parity` | Number of parity shards |
| `erasure.index` | Which shard this disk holds (0-based) |
| `erasure.distribution` | Permutation mapping shard index to disk index |
| `erasure.epoch_id` | Placement epoch recorded with the object |
| `erasure.set_id` | Placement set identity recorded with the object |
| `erasure.node_ids` | Ordered node identity for each shard |
| `erasure.disk_ids` | Ordered disk identity for each shard |
| `checksum` | SHA-256 hex of THIS shard's data (for bitrot detection) |
| `user_metadata` | Custom `x-amz-meta-*` headers from the PUT request |
| `tags` | Object tags (key-value pairs) |
| `version_id` | UUID for versioned objects, empty for unversioned |
| `is_latest` | True for the most recent non-delete-marker version |
| `is_delete_marker` | True for delete marker entries (no shard data) |

## Unversioned objects

One entry in the `versions` array. Shard data at `key/shard.dat`.
Each PUT replaces the single entry and overwrites `shard.dat`.

```
key/
  meta.json       # versions: [{ version_id: "", is_latest: true, ... }]
  shard.dat       # raw erasure shard
```

## Versioned objects

Multiple entries in the `versions` array. Each version's shard data lives
in a UUID subdirectory.

```
key/
  meta.json       # versions: [{ version_id: "uuid-2", is_latest: true }, { version_id: "uuid-1", is_latest: false }]
  uuid-2/
    shard.dat     # shard for version 2
  uuid-1/
    shard.dat     # shard for version 1
```

## Object detection

A directory is an object if it contains `meta.json`. The `walk_keys` function
in `disk.rs` uses this to list objects.

## Atomic writes

Unversioned writes: data is written directly to `key/shard.dat` and `meta.json`
is updated atomically.

Versioned writes: data is written to `key/<uuid>/shard.dat`, then `meta.json`
is updated with the new version entry prepended to the array.

## Erasure distribution

The `distribution` array maps shard indices to disk indices. It is a
deterministic mapping recorded with the object. In the single-node path it is
derived from a backend permutation. In the placement-aware path it is tied to
the active placement decision and accompanied by `epoch_id`, `set_id`,
`node_ids`, and `disk_ids`.

Example with 4 disks: `distribution: [2, 0, 3, 1]` means shard 0 is on
disk 2, shard 1 is on disk 0, shard 2 is on disk 3, shard 3 is on disk 1.

For placement-aware objects, `node_ids` and `disk_ids` make that layout stable
and externally inspectable across nodes.
