# Storage Layout

How abixio stores identity, configuration, and data on disk. This is the
single authoritative document for all on-disk metadata.

AbixIO stores all identity and configuration on the disks themselves. A fresh
binary pointed at formatted disks can reconstruct the full cluster without
external configuration.

## Design Principles

1. **No master server.** Identity is distributed across disks, not centralized.
2. **Disks are self-describing.** All identity lives on the disks themselves.
3. **Binary is disposable.** Delete it, install a fresh one, point at the disks,
   the system comes online.
4. **Disks are portable.** Move them to different hardware. The binary reads the
   format and serves.
5. **Metadata lives at the right layer.** Each layer stores exactly what belongs
   to it. No duplication, no misplaced config.

## Metadata Layers

AbixIO has four metadata layers. Each has a single responsibility.

```
Layer 1: Volume Identity       .abixio.sys/volume.json                        "Who is this volume?"
Layer 2: Cluster Runtime       .abixio.sys/cluster.json                     "What is this node doing right now?"
Layer 3: Bucket Config         .abixio.sys/buckets/<bucket>/settings.json   "How should new objects behave?"
Layer 4: Object Metadata       <bucket>/<key>/meta.json                     "What is this object and where are its shards?"
```

---

## Layer 1: Volume Identity

**File**: `.abixio.sys/volume.json` on every volume.

**Responsibility**: Permanent identity. Who is this volume, what cluster and
erasure set does it belong to, and who are all the other members.

### volume.json schema

| Field | Type | Description |
|---|---|---|
| `version` | u32 | Format schema version (currently `1`) |
| `deployment_id` | UUID | Cluster-wide identifier. Same on every disk in the cluster |
| `set_id` | UUID | Erasure set identifier. Same on every disk in the set |
| `node_id` | UUID | Node identifier. Same on every disk owned by this node |
| `volume_id` | UUID | Globally unique volume identifier |
| `volume_index` | u32 | This volume's position in the erasure set member list |
| `created_at` | u64 | Unix timestamp when this disk was formatted |
| `erasure_set.members` | array | Full set membership (see below) |

Each member in `erasure_set.members`:

| Field | Type | Description |
|---|---|---|
| `volume_id` | UUID | The member volume's unique identifier |
| `node_id` | UUID | The node that owns this member volume |
| `index` | u32 | Position in the erasure set |

**Does NOT store**:
- EC defaults (that is policy, not identity -- belongs to Layer 3 or CLI)
- Runtime state (belongs to Layer 2)
- Bucket or object config

### Example

```json
{
  "version": 1,
  "deployment_id": "a1b2c3d4-0000-0000-0000-000000000000",
  "set_id": "e5f6a7b8-0000-0000-0000-000000000000",
  "node_id": "11111111-0000-0000-0000-000000000000",
  "volume_id": "aaaaaaaa-0000-0000-0000-000000000000",
  "volume_index": 0,
  "created_at": 1712300000,
  "erasure_set": {
    "members": [
      { "volume_id": "aaaaaaaa-0000-0000-0000-000000000000", "node_id": "11111111-0000-0000-0000-000000000000", "index": 0 },
      { "volume_id": "bbbbbbbb-0000-0000-0000-000000000000", "node_id": "11111111-0000-0000-0000-000000000000", "index": 1 },
      { "volume_id": "cccccccc-0000-0000-0000-000000000000", "node_id": "22222222-0000-0000-0000-000000000000", "index": 2 },
      { "volume_id": "dddddddd-0000-0000-0000-000000000000", "node_id": "22222222-0000-0000-0000-000000000000", "index": 3 }
    ]
  }
}
```

Every disk in the cluster carries the full erasure set membership. Any single
disk is enough to reconstruct the cluster's identity.

### Identity hierarchy

```
deployment_id       one per cluster, all disks share it
  set_id            one per erasure set, all disks in the set share it
    node_id         one per node, all disks on a node share it
      volume_id       one per volume, globally unique
```

All four are UUIDv4, generated at format time, immutable after that.

### Boot sequence

**First boot (standalone)**:

1. `abixio --disks /d1,/d2,/d3,/d4 --data 2 --parity 2`
2. No volume.json found -- generate node_id, volume_id UUIDs
3. No `--peers` -- standalone mode, generate deployment_id and set_id
4. Write complete volume.json to every volume
5. Serve

**First boot (cluster)**:

1. `abixio --disks /d1,/d2 --peers http://node-2:10000`
2. No volume.json found -- generate node_id, volume_id UUIDs
3. Write partial volume.json (node_id, volume_id only)
4. Exchange identity with peers via `/_admin/cluster/join`
5. Block until all peers respond
6. Compute deployment_id and set_id deterministically from sorted node_ids
7. Build full member list, write complete volume.json
8. Serve

**Subsequent boot**:

1. Read volume.json from each volume
2. Validate all volumes agree on deployment_id, set_id, node_id
3. If `--peers` set: probe peers for quorum confirmation
4. Serve

**Volume migration**:

1. Move volumes to new hardware
2. Start abixio binary with `--disks` pointing to the moved volumes
3. Binary reads volume.json, discovers identity
4. Peers recognize the node by its persisted node_id and volume_ids
5. Node comes online

The binary never stores identity. The volumes do. Hardware is irrelevant.

### Validation rules

On boot, the binary validates volume.json across all local volumes:

| Check | Error |
|---|---|
| volume.json missing on some volumes but present on others | Mixed state: refuse to start |
| volume.json missing on all volumes, no peers | Standalone first boot: auto-format |
| volume.json missing on all volumes, peers set | Cluster first boot: peer exchange |
| deployment_id mismatch across local volumes | Corrupt or mixed cluster: refuse to start |
| set_id mismatch across local volumes | Corrupt or mixed set: refuse to start |
| node_id mismatch across local volumes | Volumes from different nodes mixed: refuse to start |
| volume_id duplicated | Corrupt: refuse to start |
| member list inconsistent across local volumes | Stale member list: warn, use newest |
| format version unsupported | Refuse to start with upgrade message |

---

## Layer 2: Cluster Runtime State

**File**: `.abixio.sys/cluster.json` on every disk.

**Responsibility**: Current operational state. Cluster summary, epoch, peer
reachability, fencing status. This is runtime state that changes frequently.

volume.json provides the identity that cluster runtime state references.
See [cluster.md](cluster.md) for the full cluster design.

---

## Layer 3: Bucket Config

**File**: `.abixio.sys/buckets/<bucket>/settings.json` on every disk.

**Responsibility**: Default behavior for new objects in this bucket. One file
per bucket, containing all bucket-level config. These are policy settings, not
identity. They influence new writes but do not affect existing objects.

```json
{
  "versioning": "Enabled",
  "ec": { "data": 3, "parity": 3 },
  "tags": { "env": "prod" },
  "policy": { "Version": "2012-10-17", "Statement": [] },
  "lifecycle": "<LifecycleConfiguration>...</LifecycleConfiguration>"
}
```

All fields are optional. Absent means "not configured, use defaults."

### EC resolution cascade

EC parameters are resolved per-request at write time using this precedence:

```
1. Per-object headers    x-amz-meta-ec-data / x-amz-meta-ec-parity   (highest)
2. Bucket default        .abixio.sys/buckets/<bucket>/settings.json
3. Server default        --data / --parity CLI flags                   (lowest)
```

The resolved data/parity values are stored in `meta.json` with the object.
After that, the object is self-describing. Changing server defaults or bucket
config does not affect existing objects.

EC config is deliberately absent from volume.json. EC is policy (how to
encode); volume.json is identity (who am I).

See [per-object-ec.md](per-object-ec.md) for full EC documentation.

---

## Layer 4: Object Metadata

**File**: `<bucket>/<key>/meta.json` on every disk that holds a shard.

**Responsibility**: Everything about this specific object version. EC params,
shard placement, checksums, user metadata, tags.

Key rule: object metadata stores **resolved** values. The EC cascade is
evaluated once at write time. The result is baked into meta.json. Reads and
heals never consult bucket config or server defaults -- they use the stored
values.

### meta.json

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
        "set_id": "e5f6a7b8-0000-0000-0000-000000000000",
        "node_ids": ["11111111-...", "22222222-...", "33333333-...", "44444444-..."],
        "volume_ids": ["aaaaaaaa-...", "cccccccc-...", "eeeeeeee-...", "gggggggg-..."]
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

The `node_ids` and `volume_ids` in object metadata use the same UUIDs as
volume.json. The healer and decoder can verify that a shard is on the correct
disk by comparing `meta.json` volume_ids against `volume.json` volume_id.

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
| `erasure.volume_ids` | Ordered disk identity for each shard |
| `checksum` | SHA-256 hex of THIS shard's data (for bitrot detection) |
| `user_metadata` | Custom `x-amz-meta-*` headers from the PUT request |
| `tags` | Object tags (key-value pairs) |
| `version_id` | UUID for versioned objects, empty for unversioned |
| `is_latest` | True for the most recent non-delete-marker version |
| `is_delete_marker` | True for delete marker entries (no shard data) |

---

## Directory Structure

AbixIO uses erasure coding across multiple disks. With `--data 2 --parity 2`,
each object is split into 4 shards distributed across 4 disks.

```
disk0/
  .abixio.sys/
    volume.json                   # disk identity (Layer 1)
    cluster.json                # cluster runtime state (Layer 2)
    buckets/
      bucket-name/
        settings.json           # bucket config (Layer 3)
    multipart/                  # in-progress multipart uploads
  bucket-name/
    object-key/
      meta.json                 # object metadata (Layer 4)
      shard.dat                 # shard data (unversioned objects)
      <uuid>/                   # shard data (versioned objects, one dir per version)
        shard.dat

disk1/
  .abixio.sys/
    volume.json                   # same deployment_id/set_id/node_id, different volume_id
    cluster.json
    buckets/
      bucket-name/
        settings.json
  bucket-name/
    object-key/
      meta.json                 # same structure, different shard index/checksum
      shard.dat
```

All system metadata lives under `.abixio.sys/`. Bucket data directories contain
only user objects.

Each disk holds one shard per object. The shard index and checksum differ per
disk, but the rest of the metadata is identical.

### Unversioned objects

One entry in the `versions` array. Shard data at `key/shard.dat`. Each PUT
replaces the single entry and overwrites `shard.dat`.

```
key/
  meta.json       # versions: [{ version_id: "", is_latest: true, ... }]
  shard.dat       # raw erasure shard
```

### Versioned objects

Multiple entries in the `versions` array. Each version's shard data lives in a
UUID subdirectory.

```
key/
  meta.json       # versions: [{ version_id: "uuid-2", is_latest: true }, { version_id: "uuid-1", is_latest: false }]
  uuid-2/
    shard.dat     # shard for version 2
  uuid-1/
    shard.dat     # shard for version 1
```

### Object detection

A directory is an object if it contains `meta.json`. The `walk_keys` function
in `disk.rs` uses this to list objects.

### Atomic writes

Unversioned writes: data is written directly to `key/shard.dat` and `meta.json`
is updated atomically.

Versioned writes: data is written to `key/<uuid>/shard.dat`, then `meta.json`
is updated with the new version entry prepended to the array.

### Erasure distribution

The `distribution` array maps shard indices to disk indices. It is a
deterministic mapping recorded with the object. In the single-node path it is
derived from a backend permutation. In the placement-aware path it is tied to
the active placement decision and accompanied by `epoch_id`, `set_id`,
`node_ids`, and `volume_ids`.

Example with 4 disks: `distribution: [2, 0, 3, 1]` means shard 0 is on
disk 2, shard 1 is on disk 0, shard 2 is on disk 3, shard 3 is on disk 1.

For placement-aware objects, `node_ids` and `volume_ids` make that layout stable
and externally inspectable across nodes.

---

## CLI

| Flag | Required | Default | Purpose |
|---|---|---|---|
| `--disks` | yes | -- | Comma-separated volume paths |
| `--data` | no | 1 | Server default data shards |
| `--parity` | no | 0 | Server default parity shards |
| `--listen` | no | `:10000` | Bind address |
| `--peers` | no | empty | Peer endpoints for cluster mode |
| `--cluster-secret` | no | empty | Shared secret for peer probes |
| `--no-auth` | no | false | Disable S3 authentication |

Node identity is read from volume.json. Cluster membership is established via
peer exchange at startup.
