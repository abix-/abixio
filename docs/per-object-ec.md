# Per-Object Erasure Coding

Every abixio object has its own fault tolerance. Critical files can survive
5 disk failures while bulk logs tolerate only 1, all within the same bucket
on the same disk pool.

## Failures to tolerate (FTT)

The primary interface is FTT: a single number saying how many volume failures
an object can survive. The system computes the shard layout from FTT and the
available disk count.

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

AbixIO stores `erasure.ftt` and `erasure.volume_ids` in every object's `meta.json`.
The encode path resolves EC params per-request using a precedence chain. The
decode and heal paths read EC params from the stored metadata, so objects with
different EC ratios coexist seamlessly.

Placement-aware objects also record:

- `epoch_id`
- `pool_id`
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

## Bucket FTT

Every bucket gets FTT=1 at creation. Change it via the admin API. See [admin-api.md](admin-api.md).

### Example configurations

| Pool | Bucket FTT | Object FTT | EC layout | Fault tolerance |
|---|---|---|---|---|
| 4 disks | 1 (default) | -- | 3+1 | 1 failure |
| 6 disks | 1 (default) | -- | 5+1 | 1 failure |
| 6 disks | 1 | FTT=5 | 1+5 | 5 failures |
| 6 disks | 2 | -- | 4+2 | 2 failures |
| 8 disks | 1 (default) | -- | 7+1 | 1 failure |

## How reads work

The decode path reads `erasure.ftt` and `erasure.volume_ids` from the object's
`meta.json`. It does not consult bucket FTT. Objects are self-describing:
changing bucket FTT or adding disks does not affect existing objects.

## Related

- [healing.md](healing.md) for how the heal worker uses per-object FTT
- [comparison.md](comparison.md) for how this differs from MinIO's model
- [storage-layout.md](storage-layout.md) for on-disk metadata format
