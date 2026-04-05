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
Layer 1: Disk Identity         .abixio.sys/format.json        "Who is this disk?"
Layer 2: Cluster Runtime       .abixio.sys/cluster/state.json "What is this node doing right now?"
Layer 3: Bucket Config         <bucket>/.ec.json, etc.        "How should new objects behave?"
Layer 4: Object Metadata       <bucket>/<key>/meta.json       "What is this object and where are its shards?"
```

---

## Layer 1: Disk Identity

**File**: `.abixio.sys/format.json` on every disk.

**Responsibility**: Permanent identity. Who is this disk, what cluster and
erasure set does it belong to, and who are all the other members.

### format.json schema

| Field | Type | Description |
|---|---|---|
| `version` | u32 | Format schema version (currently `1`) |
| `deployment_id` | UUID | Cluster-wide identifier. Same on every disk in the cluster |
| `set_id` | UUID | Erasure set identifier. Same on every disk in the set |
| `node_id` | UUID | Node identifier. Same on every disk owned by this node |
| `disk_id` | UUID | Globally unique disk identifier |
| `disk_index` | u32 | This disk's position in the erasure set member list |
| `created_at` | u64 | Unix timestamp when this disk was formatted |
| `erasure_set.members` | array | Full set membership (see below) |

Each member in `erasure_set.members`:

| Field | Type | Description |
|---|---|---|
| `disk_id` | UUID | The member disk's unique identifier |
| `node_id` | UUID | The node that owns this member disk |
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
  "disk_id": "aaaaaaaa-0000-0000-0000-000000000000",
  "disk_index": 0,
  "created_at": 1712300000,
  "erasure_set": {
    "members": [
      { "disk_id": "aaaaaaaa-0000-0000-0000-000000000000", "node_id": "11111111-0000-0000-0000-000000000000", "index": 0 },
      { "disk_id": "bbbbbbbb-0000-0000-0000-000000000000", "node_id": "11111111-0000-0000-0000-000000000000", "index": 1 },
      { "disk_id": "cccccccc-0000-0000-0000-000000000000", "node_id": "22222222-0000-0000-0000-000000000000", "index": 2 },
      { "disk_id": "dddddddd-0000-0000-0000-000000000000", "node_id": "22222222-0000-0000-0000-000000000000", "index": 3 }
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
      disk_id       one per physical disk, globally unique
```

All four are UUIDv4, generated at format time, immutable after that.

### Format lifecycle

**First boot (standalone)**:

1. Operator starts `abixio --disks /d1,/d2,/d3,/d4 --data 2 --parity 2`
2. Binary checks each disk for `.abixio.sys/format.json`
3. No format found on any disk -- this is a first boot
4. Generate UUIDs: one `deployment_id`, one `set_id`, one `node_id`, one
   `disk_id` per disk
5. Write `format.json` to every disk with the full member list
6. Proceed to serve

On subsequent boots, the binary reads format.json and derives identity. No
`--node-id` needed. `--data`/`--parity` are server-default EC policy.

**First boot (multi-node, topology-seeded)**:

Multi-node clusters need all nodes to agree on deployment_id, set_id, and each
other's identities. For v1, a static topology file seeds the initial format.

1. Operator writes a topology file with node entries and disk paths
2. Each node starts with `abixio --disks ... --cluster-topology topology.json`
3. Binary checks disks for format.json -- none found
4. Binary reads the topology file and matches this node by disk paths:
   - Normalize local `--disks` paths
   - Find the topology node whose disk paths match
   - Error if zero matches or multiple matches
5. Generate UUIDs: `deployment_id` and `set_id` are derived deterministically
   from the topology file's `cluster_id` and `set_id` (so all nodes generate
   the same values independently). `node_id` and `disk_id` are derived
   deterministically from the topology node_id/disk_id strings (so the same
   topology always produces the same UUIDs)
6. Write format.json to every local disk with the full set membership
7. Proceed to serve

After formatting, the topology file is no longer required. Subsequent boots
read identity from disk. If the topology file is still present, it is
validated against the on-disk format as a safety check.

**Subsequent boot**:

1. Binary reads format.json from each disk
2. Validates all disks agree on deployment_id, set_id, node_id
3. Derives identity from format -- no CLI identity flags needed
4. If `--cluster-topology` is present, validate it matches on-disk format
5. Serve

**Disk migration**:

1. Physically move disks to new hardware
2. Start abixio binary with `--disks` pointing to the moved disks
3. Binary reads format.json, discovers this node's identity
4. Peers recognize the node by its persisted node_id and disk_ids
5. Node comes online

The binary never stored identity. The disks did. Hardware is irrelevant.

**Disk replacement**:

1. New blank disk has no format.json
2. Operator (or future automation) formats the new disk with:
   - Same deployment_id, set_id, node_id
   - New disk_id (UUIDv4)
   - Updated member list
3. All other disks in the set update their member lists (epoch bump)
4. Healer reconstructs missing shards onto the new disk

### Validation rules

On boot, the binary validates format.json across all local disks:

| Check | Error |
|---|---|
| format.json missing on some disks but present on others | Mixed state: refuse to start |
| format.json missing on all disks, no topology | Standalone first boot: auto-format |
| format.json missing on all disks, topology present | Cluster first boot: topology-seeded format |
| deployment_id mismatch across local disks | Corrupt or mixed cluster: refuse to start |
| set_id mismatch across local disks | Corrupt or mixed set: refuse to start |
| node_id mismatch across local disks | Disks from different nodes mixed together: refuse to start |
| disk_id duplicated | Corrupt: refuse to start |
| member list inconsistent across local disks | Stale member list: warn, use newest |
| format version unsupported | Refuse to start with upgrade message |

### Future: peer-negotiated format

The topology-seeded approach requires a config file for multi-node setup. A
future improvement:

1. Nodes discover each other via `--peers`
2. Exchange disk inventories
3. Deterministic leader election picks a coordinator
4. Coordinator assigns deployment_id, set_id, generates member list
5. All nodes write format.json
6. No config file at all

This is more complex but eliminates the topology file entirely. The
topology-seeded approach is the right v1 because the static topology model
already exists and works.

---

## Layer 2: Cluster Runtime State

**File**: `.abixio.sys/cluster/state.json` on every disk.

**Responsibility**: Current operational state. Cluster summary, epoch, peer
reachability, fencing status. This is runtime state that changes frequently.

format.json provides the identity that cluster runtime state references.
See [cluster.md](cluster.md) for the full cluster design.

---

## Layer 3: Bucket Config

**Files**: Per-bucket config files at the bucket directory root on every disk.

| File | Purpose |
|---|---|
| `.versioning.json` | Versioning state (`Enabled` / `Suspended`) |
| `.tagging.json` | Bucket-level tags |
| `.ec.json` | Bucket EC default (data/parity override) |
| `.policy.json` | Bucket policy (storage only, enforcement pending) |

**Responsibility**: Default behavior for new objects in this bucket. These are
policy files, not identity. They influence new writes but do not affect
existing objects.

### EC resolution cascade

EC parameters are resolved per-request at write time using this precedence:

```
1. Per-object headers    x-amz-meta-ec-data / x-amz-meta-ec-parity   (highest)
2. Bucket default        <bucket>/.ec.json
3. Server default        --data / --parity CLI flags                   (lowest)
```

The resolved data/parity values are stored in `meta.json` with the object.
After that, the object is self-describing. Changing server defaults or bucket
config does not affect existing objects.

EC config is deliberately absent from format.json. EC is policy (how to
encode); format.json is identity (who am I).

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
        "disk_ids": ["aaaaaaaa-...", "cccccccc-...", "eeeeeeee-...", "gggggggg-..."]
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

The `node_ids` and `disk_ids` in object metadata use the same UUIDs as
format.json. The healer and decoder can verify that a shard is on the correct
disk by comparing `meta.json` disk_ids against `format.json` disk_id.

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

---

## Directory Structure

AbixIO uses erasure coding across multiple disks. With `--data 2 --parity 2`,
each object is split into 4 shards distributed across 4 disks.

```
disk0/
  .abixio.sys/
    format.json                 # disk identity (Layer 1)
    cluster/
      state.json                # cluster runtime state (Layer 2)
    multipart/                  # in-progress multipart uploads
  bucket-name/
    .versioning.json            # optional: bucket versioning config (Layer 3)
    .tagging.json               # optional: bucket tags (Layer 3)
    .ec.json                    # optional: bucket EC default (Layer 3)
    object-key/
      meta.json                 # object metadata (Layer 4)
      shard.dat                 # shard data (unversioned objects)
      <uuid>/                   # shard data (versioned objects, one dir per version)
        shard.dat

disk1/
  .abixio.sys/
    format.json                 # same deployment_id/set_id/node_id, different disk_id
    cluster/
      state.json
  bucket-name/
    object-key/
      meta.json                 # same structure, different shard index/checksum
      shard.dat
```

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
`node_ids`, and `disk_ids`.

Example with 4 disks: `distribution: [2, 0, 3, 1]` means shard 0 is on
disk 2, shard 1 is on disk 0, shard 2 is on disk 3, shard 3 is on disk 1.

For placement-aware objects, `node_ids` and `disk_ids` make that layout stable
and externally inspectable across nodes.

---

## CLI Changes

| Current | After disk format |
|---|---|
| `--node-id node-1` (required for cluster) | Read from format.json (removed from CLI) |
| `--cluster-topology` (required every boot) | One-time format seed, optional after |
| `--data 2 --parity 2` (server default EC) | Unchanged -- policy, not identity |
| `--disks /d1,/d2` | Unchanged -- tells binary which paths to use |

A `--node-id-override` flag may be kept for disaster recovery (force identity
regardless of on-disk format).
