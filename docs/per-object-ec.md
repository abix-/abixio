# Per-Object Erasure Coding

Every abixio object has its own data/parity ratio. Critical files can be stored
with maximum redundancy while large files use throughput-optimized settings --
within the same bucket, on the same disk pool.

## How it works

AbixIO stores `erasure.data` and `erasure.parity` in every object's `meta.json`.
The encode path resolves EC params per-request using a precedence chain. The
decode and heal paths read EC params from the stored metadata, so objects with
different EC ratios coexist seamlessly.

Placement-aware objects also record:

- `epoch_id`
- `set_id`
- `node_ids`
- `volume_ids`

Those fields identify where each shard belongs, in addition to the EC ratio
itself.

### Precedence chain

| Priority | Source | How to set |
|---|---|---|
| 1 (highest) | Per-object | `x-amz-meta-ec-data` / `x-amz-meta-ec-parity` headers on PUT |
| 2 | Bucket default | Admin API: `PUT /_admin/bucket/{name}/ec?data=N&parity=N` |
| 3 (lowest) | Server default | Auto-computed from volume count |

If no override is specified, objects use the auto-computed server default:
1 volume = `1+0`, 2+ volumes = `(N-1)+1` for 1-failure tolerance.

## Per-object EC via S3 headers

Standard S3 `x-amz-meta-*` custom metadata headers. Any S3 client works.

```bash
# critical config: 1 data + 5 parity (survives 5 of 6 disk failures)
curl -X PUT -d "critical data" \
  -H "x-amz-meta-ec-data: 1" \
  -H "x-amz-meta-ec-parity: 5" \
  http://localhost:10000/mybucket/critical.txt

# large video: 4 data + 2 parity (throughput-optimized)
curl -X PUT -T bigfile.bin \
  -H "x-amz-meta-ec-data: 4" \
  -H "x-amz-meta-ec-parity: 2" \
  http://localhost:10000/mybucket/bigfile.bin

# no headers: uses bucket default or server default
curl -X PUT -d "normal data" \
  http://localhost:10000/mybucket/normal.txt
```

The EC headers are returned on GET and HEAD like all S3 custom metadata.

### Validation

- `data` must be >= 1
- `data + parity` must be <= total disk count in the pool
- Invalid values return HTTP 400 `InvalidRequest`

## Bucket EC config

Set a default EC ratio for all new objects in a bucket. Stored in
`.abixio.sys/buckets/<bucket>/settings.json` under the `ec` field.

### Admin API

```bash
# set bucket default
curl -X PUT "http://localhost:10000/_admin/bucket/mybucket/ec?data=3&parity=3"

# get bucket config (returns auto-computed default if not set)
curl http://localhost:10000/_admin/bucket/mybucket/ec
```

Response format:

```json
{
  "data": 3,
  "parity": 3
}
```

If no bucket config is set, the GET endpoint returns the auto-computed server
defaults with `"source": "server_default"`.

## Volume pool model

AbixIO volumes form a pool. The auto-computed server default uses all volumes
(e.g., 6 volumes = `5+1`). Per-bucket EC can select a smaller subset.

```bash
# 6-volume pool, auto-computes 5+1 default
abixio --volumes /d{1...6}

# set bucket to use 2+2 instead (4 of 6 volumes per object)
curl -X PUT "http://localhost:10000/_admin/bucket/mybucket/ec?data=2&parity=2"
```

Each object selects a subset of volumes from the pool.

In the single-node path this is a deterministic backend subset. In the
placement-aware path the same EC choice is combined with stable placement
metadata.

Single-node example:

```
full_permutation = hash_order("bucket/key", 6)  # e.g. [3,1,0,5,2,4]
selected_disks = full_permutation[..4]            # [3,1,0,5] for data=2,parity=2
```

Different objects land on different disk subsets, spreading I/O across the pool.

### Example configurations

| Pool | Default EC | Object EC | Disks used | Fault tolerance |
|---|---|---|---|---|
| 4 disks | 2+2 | (default) | 4 of 4 | 2 failures |
| 6 disks | 2+2 | (default) | 4 of 6 | 2 failures |
| 6 disks | 2+2 | 1+5 | 6 of 6 | 5 failures |
| 6 disks | 2+2 | 4+2 | 6 of 6 | 2 failures, 2x throughput |
| 8 disks | 2+2 | 1+1 | 2 of 8 | 1 failure, minimal overhead |

## How reads work

The decode path reads `erasure.data` and `erasure.parity` from the object's
`meta.json`. It does not use the server or bucket defaults. This means:

- Objects written with EC 1+5 are always read with EC 1+5
- Changing volume count does not affect existing objects
- Objects from before per-object EC still work (their `meta.json` records the
  EC params they were written with)

## How healing works

The heal worker reads EC params from each object's stored metadata. A pool can
contain objects with different EC ratios, and the healer handles each one
correctly:

1. Read `meta.json` from any available disk
2. Extract `erasure.data` and `erasure.parity`
3. Use stored `distribution` and, when present, stored placement identity to identify which shards belong where
4. Reconstruct missing shards using the object's own EC params
5. Write repaired shards back

## Comparison with MinIO

| Aspect | MinIO | AbixIO |
|---|---|---|
| EC granularity | Per server pool | Per object |
| Changing EC | Requires new server pool | Set header on next PUT |
| Mixed EC in same bucket | Not possible | Native |
| EC stored in metadata | Yes (xl.meta) | Yes (meta.json) |
| Disk selection | All disks in erasure set | Deterministic subset of pool, optionally with placement metadata |

MinIO's approach requires planning EC ratios at deployment time. AbixIO lets
you choose per object, per bucket, or per server -- and change your mind at
any time for new objects.

## Disk layout

```
disk0/
  .abixio.sys/
    buckets/
      mybucket/
        settings.json     # { "ec": { "data": 3, "parity": 3 } }
  mybucket/
    critical.txt/
      meta.json           # erasure: { data: 1, parity: 5, ... }
      shard.dat
    bigfile.bin/
      meta.json           # erasure: { data: 4, parity: 2, ... }
      shard.dat
```

Each object's `meta.json` is the source of truth for its EC configuration.
The bucket-level `settings.json` EC config is only used as a default for new
writes.

For placement-aware objects, `meta.json` also records `epoch_id`, `set_id`,
`node_ids`, and `volume_ids` alongside `data`, `parity`, `index`, and
`distribution`.
