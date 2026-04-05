# Cluster Design

AbixIO now has an initial cluster-control layer for multi-node deployments.
This document explains what exists today, what behavior is enforced, and what
is still planned.

This is intentionally a control-plane-first design. The current implementation
adds cluster identity, persisted cluster metadata, peer monitoring, admin
visibility, hard fencing, and deterministic node-aware placement metadata. It
does **not** yet add internode shard RPC or consensus-backed topology
reconfiguration.

## Goals

The cluster layer is designed around these rules:

1. **Nodes start from a host list, not a full static disk map.**
   Each node knows its own local disks and the peer endpoints it should talk to.

2. **Cluster state is explicit.**
   Nodes persist cluster metadata under the internal AbixIO metadata area rather
   than treating clustering as a purely in-memory concern.

3. **Control-plane safety is more important than stale availability.**
   If a node loses cluster quorum, it must stop serving.

4. **All nodes remain real storage and S3 nodes.**
   The cluster layer is not a separate external service. The same `abixio`
   process serves S3, healing, admin endpoints, and cluster control behavior.

## Current Scope

The current implementation provides:

- cluster identity and peer configuration via CLI flags
- persisted cluster state on local disks
- admin endpoints for cluster summary, nodes, epochs, and topology
- peer probing and basic quorum tracking
- hard fencing when the node is not in a `ready` cluster state
- S3 request rejection while fenced
- mutating admin request rejection while fenced
- deterministic placement planning metadata with stable `epoch_id`, `set_id`,
  `node_ids`, and `disk_ids` stored in object shard metadata
- 4-node, 2-disk distributed placement tests that validate exact object maps,
  node-first shard spread, and fencing behavior

The current implementation does **not** provide:

- Raft or another real distributed consensus log
- production internode shard writes or distributed reads over RPC
- remote disk backends over internode RPC
- committed topology epochs negotiated across the cluster
- heterogeneous set-class planning
- rebalance or topology migration

That means the cluster layer now has a real placement model and test harness,
but production networking is still a strict safety and visibility boundary
rather than a full remote object-store data plane.

## CLI Configuration

The cluster control layer adds these server options:

| Flag | Meaning |
|---|---|
| `--node-id` | Stable node identity used in cluster metadata |
| `--cluster-bind` | Cluster bind or advertised coordination address |
| `--advertise-s3` | Public S3 endpoint for this node |
| `--advertise-cluster` | Public cluster-control endpoint for this node |
| `--peers` | Comma-separated peer host list |
| `--cluster-secret` | Shared secret for cluster peer probes |

Example:

```bash
./target/release/abixio \
  --listen 0.0.0.0:9000 \
  --cluster-bind 0.0.0.0:9000 \
  --advertise-s3 http://node1.lan:9000 \
  --advertise-cluster http://node1.lan:9000 \
  --node-id node1 \
  --peers http://node2.lan:9000,http://node3.lan:9000 \
  --cluster-secret supersecret \
  --disks /srv/abixio/d1,/srv/abixio/d2,/srv/abixio/d3,/srv/abixio/d4 \
  --data 2 --parity 2
```

For a standalone node, omit `--peers`. In that mode the node immediately
transitions to `ready` and does not require quorum.

## Persisted State

Cluster metadata is stored under:

```text
<disk>/.abixio.sys/cluster/state.json
```

The server writes the same cluster-state JSON file to every local disk it owns.
This is local persistence only. It is not yet a distributed commit log.

The persisted state includes:

- cluster summary
- current epoch metadata
- node view
- disk view
- epoch history

Object shard metadata now also persists placement identity:

- `epoch_id`
- `set_id`
- ordered shard `node_ids`
- ordered shard `disk_ids`

This lets admin inspection and tests validate exact placement instead of only
local disk indexes.

## Service States

The cluster layer exposes four service states:

| State | Meaning |
|---|---|
| `joining` | Node has not converged enough to serve |
| `syncing_epoch` | Node is initializing cluster metadata |
| `fenced` | Node must not serve S3 or mutate data |
| `ready` | Node is allowed to serve |

### Fencing

Fencing is the most important safety rule in the current design.

If the cluster manager decides the node is not safe to serve, the node enters
`fenced` and:

- rejects all S3 traffic with `503 ServiceUnavailable`
- rejects mutating admin operations such as manual heal and bucket EC changes
- reports the fenced reason in admin status

This matches the intended rule for AbixIO clustering: if a node loses contact
with the cluster control plane, it must stop serving instead of risking stale
or split-brain behavior.

## Quorum Model Today

Today the cluster manager uses a simple peer-probe model:

1. Load local persisted cluster state.
2. Probe configured peers over `/_admin/cluster/status`.
3. Count reachable voters.
4. Compute quorum as `voter_count / 2 + 1`.
5. Enter `ready` only if reachable voters meet quorum.
6. Enter `fenced` if quorum is lost.

This is a deliberate interim model. It gives AbixIO real operational behavior
around cluster readiness and safety, but it is not yet a durable distributed
decision protocol.

## Admin API

The cluster layer adds these admin endpoints:

| Endpoint | Method | Meaning |
|---|---|---|
| `/_admin/cluster/status` | GET | Cluster summary and current topology |
| `/_admin/cluster/nodes` | GET | Current node view |
| `/_admin/cluster/epochs` | GET | Current and historical epochs |
| `/_admin/cluster/topology` | GET | Current topology view |

`/_admin/status` also now includes a `cluster` section so existing tooling can
see whether the node is `ready` or `fenced`.

`/_admin/object` now reports placement identity per shard:

- shard index
- local backend index
- `node_id`
- `disk_id`
- shard status and checksum

### Peer Probe Authentication

Peer probes use the `x-abixio-cluster-secret` header when `--cluster-secret` is
configured. This is only a lightweight internode gate for the current control
plane. It is not a full cluster PKI or mTLS design.

## Placement Model

The current planner assumes a homogeneous set and uses a deterministic
node-first spread rule:

1. Hash the object identity together with the committed epoch and set ID.
2. Order nodes deterministically from that hash.
3. Place at most one shard on a node before using a second disk on any node.
4. Within each node, order its disks deterministically and consume them in
   rounds.

For a 4-node, 2-disk topology this means:

- a `2+2` object lands on four different nodes
- a `4+2` object lands on all four nodes, then uses one additional disk on two
  nodes
- no placement repeats a disk within one object

This is the exact rule validated by the distributed placement tests.

## Test Coverage

The distributed placement suite currently covers:

- deterministic planner behavior
- exhaustive EC tuple validation for the 4x2 topology
- placement spread across nodes before disk reuse
- approximate long-run balance across nodes and disks
- exact object placement visibility through `/_admin/object`
- raw on-disk validation that shards exist only on the expected disks
- one-node failure tolerance for `2+2`
- fencing behavior when cluster quorum is intentionally lost

The multi-node integration harness uses shared local disk roots plus per-node
availability gates to simulate four independent nodes on one machine. That is a
test harness, not production RPC.

## Relationship To The Future Design

The intended long-term distributed design is still:

- nodes self-register
- the cluster computes authoritative topology epochs
- heterogeneous disks are grouped into compatible placement structures
- objects are assigned to explicit placement groups
- all data-path behavior is gated by committed cluster state

To reach that design, AbixIO still needs:

1. A real consensus-backed control-plane log
2. Internode RPC for storage and metadata operations
3. Authoritative node and disk registration
4. Topology planning for heterogeneous nodes
5. Placement metadata stored with objects
6. Reconfiguration and rebalance workflows

The current cluster layer should be treated as the safety scaffold that those
features will build on.

## Operational Notes

- Standalone mode remains supported and automatically reports `ready`.
- Multi-peer mode should be treated as experimental control-plane behavior until
  consensus and remote storage RPC are implemented.
- A fenced node is working as designed. It is refusing service to avoid
  inconsistent behavior.
- Cluster status can be checked without S3 requests through the admin endpoints.

## Recommended Reading

- [architecture.md](architecture.md) for the overall storage model
- [admin-api.md](admin-api.md) for the admin endpoints
- [per-object-ec.md](per-object-ec.md) for current erasure-coding behavior
- [storage-layout.md](storage-layout.md) for on-disk object metadata layout
