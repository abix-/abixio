# Per-Object Erasure Coding

Every abixio object has its own fault tolerance. Critical files can survive
5 disk failures while bulk logs tolerate only 1, all within the same bucket
on the same disk pool.

## Failures to tolerate (FTT)

The primary interface is FTT: a single number saying how many volume failures
an object can survive. The system computes optimal data/parity shards from
FTT and the available disk count.

| Disks | FTT=0 | FTT=1 | FTT=2 | FTT=3 |
|---|---|---|---|---|
| 1 | 1+0 | invalid | invalid | invalid |
| 2 | 2+0 | 1+1 | invalid | invalid |
| 4 | 4+0 | 3+1 | 2+2 | 1+3 |
| 6 | 6+0 | 5+1 | 4+2 | 3+3 |

FTT means volume/disk failures. Node-awareness is handled separately by the
placement engine, which round-robins shards across nodes before reusing a
node. In single-node mode, FTT means local disk failures.

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
| 1 (highest) | Per-object FTT | `x-amz-meta-ec-ftt` header on PUT |
| 2 (lowest) | Bucket FTT | Auto-assigned at bucket creation (FTT=1), changeable via admin API |

Every bucket gets FTT=1 at creation (FTT=0 for single-disk deployments).

## Per-object EC via S3 headers

Standard S3 `x-amz-meta-*` custom metadata headers. Any S3 client works.

```bash
# critical config: survive 5 failures (FTT=5 on 6 disks -> 1+5)
curl -X PUT -d "critical data" \
  -H "x-amz-meta-ec-ftt: 5" \
  http://localhost:10000/mybucket/critical.txt

# bulk data: survive 1 failure (FTT=1 on 6 disks -> 5+1)
curl -X PUT -T bigfile.bin \
  -H "x-amz-meta-ec-ftt: 1" \
  http://localhost:10000/mybucket/bigfile.bin

# no headers: uses bucket FTT (default: 1)
curl -X PUT -d "normal data" \
  http://localhost:10000/mybucket/normal.txt
```

The FTT header is returned on GET and HEAD like all S3 custom metadata.

### Validation

- FTT must be >= 0 and < total disk count
- Invalid values return HTTP 400 `InvalidRequest`

## Bucket EC config

Set a default EC for all new objects in a bucket. Stored in
`.abixio.sys/buckets/<bucket>/settings.json` under the `ec` field.

### Admin API

```bash
# set bucket default
curl -X PUT "http://localhost:10000/_admin/bucket/mybucket/ec?ftt=2"

# get bucket config (returns auto-computed default if not set)
curl http://localhost:10000/_admin/bucket/mybucket/ec
```

Response format:

```json
{
  "data": 4,
  "parity": 2,
  "ftt": 2
}
```

Every bucket gets FTT=1 at creation. The GET endpoint always returns a config.

## Volume pool model

AbixIO volumes form a pool. FTT determines the data/parity split. All volumes
are used for every object.

```bash
# 6-volume pool, bucket default FTT=1 -> 5+1
abixio --volumes /d{1...6}

# set bucket to FTT=2 -> 4+2
curl -X PUT "http://localhost:10000/_admin/bucket/mybucket/ec?ftt=2"
```

In the single-node path, shard distribution is a deterministic backend
permutation. In the placement-aware path, the same FTT choice is combined
with stable placement metadata including `epoch_id`, `set_id`, `node_ids`,
and `volume_ids`.

### Example configurations

| Pool | Bucket FTT | Object FTT | EC layout | Fault tolerance |
|---|---|---|---|---|
| 4 disks | 1 (default) | -- | 3+1 | 1 failure |
| 6 disks | 1 (default) | -- | 5+1 | 1 failure |
| 6 disks | 1 | FTT=5 | 1+5 | 5 failures |
| 6 disks | 2 | -- | 4+2 | 2 failures |
| 8 disks | 1 (default) | -- | 7+1 | 1 failure |

## How reads work

The decode path reads `erasure.data` and `erasure.parity` from the object's
`meta.json`. It does not consult bucket FTT. This means:

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
        settings.json     # { "ec": { "data": 4, "parity": 2, "ftt": 2 } }
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
