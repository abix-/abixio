# Cluster Design

AbixIO has a node-based cluster-control layer for multi-node deployments.
Nodes generate their own identity, exchange it with nodes at startup, and
persist the full membership on their volumes. No topology file, no master
server.

This is intentionally a control-plane-first design. The current implementation
adds node-based identity exchange, persisted cluster metadata, self-describing
volumes, node monitoring, admin visibility, hard fencing, deterministic
node-aware placement metadata, and internode shard RPC over HTTP. It does
**not** yet add consensus-backed topology reconfiguration.

## Goals

The cluster layer is designed around these rules:

1. **Volumes are self-describing.**
   Each volume carries `.abixio.sys/volume.json` with its node_id, volume_id,
   cluster_id, and the full pool membership. A fresh binary pointed
   at formatted volumes can reconstruct the cluster.

2. **No master server.**
   Nodes discover each other via `--nodes` and exchange identity at startup.
   There is no central coordinator or topology file.

3. **Cluster state is explicit.**
   Nodes persist cluster metadata under `.abixio.sys/` rather than treating
   clustering as a purely in-memory concern.

4. **Control-plane safety is more important than stale availability.**
   If a node loses cluster quorum, it must stop serving.

5. **All nodes remain real storage and S3 nodes.**
   The cluster layer is not a separate external service. The same `abixio`
   process serves S3, healing, admin endpoints, and cluster control behavior.

## Current Scope

The current implementation provides:

- node-based identity exchange at startup via `/_admin/cluster/join`
- self-describing volumes with `.abixio.sys/volume.json`
- persisted cluster state on local volumes
- admin endpoints for cluster summary, nodes, epochs, and topology
- node probing and basic quorum tracking
- hard fencing when the node is not in a `ready` cluster state
- S3 request rejection while fenced
- mutating admin request rejection while fenced
- deterministic placement metadata with `epoch_id` and `volume_ids` stored in object shard metadata
- internode shard RPC via `RemoteVolume` over HTTP (`/_storage/v1/*`)
- JWT-authenticated internode requests (signed with S3 credentials)
- mixed local + remote backend construction after identity exchange
- 4-node, 2-volume distributed placement tests that validate exact object maps,
  node-first shard spread, and fencing behavior

The current implementation does **not** provide:

- Raft or another real distributed consensus log
- live committed topology epochs negotiated across the cluster
- heterogeneous set-class planning
- rebalance or topology migration

## CLI Configuration

| Flag | Required | Default | Purpose |
|---|---|---|---|
| `--volumes` | yes | none | Volume paths (comma-separated, supports `{N...M}`) |
| `--listen` | no | `:10000` | Bind address |
| `--nodes` | no | empty | All node endpoints (supports `{N...M}`) |
| `--no-auth` | no | false | Disable all authentication |

Example:

```bash
# same command on every node, only --volumes differs per node
./target/release/abixio \
  --volumes /srv/abixio/d{1...2} \
  --nodes http://node{1...3}:10000
```

For a standalone node, omit `--nodes`. The node immediately transitions to
`ready` and does not require quorum.

## Boot Sequence

1. Read `.abixio.sys/volume.json` from each `-v` path
2. If no volume.json exists (first boot): generate node_id and volume_id UUIDs,
   write partial volume.json
3. If `--nodes` is empty: standalone mode, finalize volume.json immediately
4. If `--nodes` is set: exchange identity with nodes via `/_admin/cluster/join`,
   block until all nodes respond, then finalize volume.json with full membership
5. On subsequent boots: read identity from volume.json, probe nodes for quorum

## Persisted State

Cluster metadata is stored under:

```text
<disk>/.abixio.sys/cluster.json
```

The server writes the same cluster-state JSON file to every local disk it owns.
This is local persistence only. It is not yet a distributed commit log.

The persisted state includes:

- cluster summary
- current epoch metadata
- node view
- disk view
- current epoch snapshot
- loaded topology hash when a manifest is configured

Object shard metadata now also persists placement identity:

- `epoch_id`
- ordered shard `volume_ids`

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

Today the cluster manager uses a simple node-probe model on top of the static
manifest:

1. Load local persisted cluster state.
2. Probe configured nodes over `/_admin/cluster/status`.
3. Count reachable voters.
4. Compute quorum as `voter_count / 2 + 1`.
5. Enter `ready` only if reachable voters meet quorum.
6. Enter `fenced` if quorum is lost.

This is a deliberate interim model. It gives AbixIO real operational behavior
around cluster readiness and safety, but it is not yet a durable distributed
decision protocol.

The manifest supplies cluster identity and expected topology. The probe model
only answers whether the node currently considers that topology safe enough to
serve.

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
- `volume_id`
- shard status and checksum

### Internode Authentication

All internode communication uses JWT signed with the S3 credentials
(`ABIXIO_ACCESS_KEY` / `ABIXIO_SECRET_KEY`). This covers both cluster
probes and storage RPC.

Each request carries:
- `Authorization: Bearer <jwt>`: signed with `ABIXIO_SECRET_KEY`, 15min expiry
- `x-abixio-time: <unix_nanos>`: clock skew detection (rejects >15min drift)

When `--no-auth` is set, JWT validation is skipped (development/trusted network).

## Placement Model

The current planner assumes a homogeneous set and uses a deterministic
node-first spread rule:

1. Hash the object identity together with the active epoch and set ID.
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

## Future Design

AbixIO still needs:

1. Topology planning for heterogeneous nodes
2. Reconfiguration and rebalance workflows
3. A real consensus-backed control-plane log (Raft or equivalent)

The current cluster layer should be treated as the minimal safe operating model
and the scaffold those features would build on.

## Operational Notes

- Standalone mode remains supported and automatically reports `ready`.
- Node-based cluster mode is the primary clustered configuration.
- Internode shard RPC is implemented: `RemoteVolume` proxies all `Backend`
  operations over HTTP to the target node's `StorageServer`.
- A fenced node is working as designed. It is refusing service to avoid
  inconsistent behavior.
- Cluster status can be checked without S3 requests through the admin endpoints.

## Volume Identity

Each volume carries `.abixio.sys/volume.json` with deployment, set, node, and
volume UUIDs plus the full pool membership. Identity is generated at
first boot and exchanged with nodes. A fresh binary pointed at formatted
volumes can reconstruct the cluster without any config files.

See [storage-layout.md](storage-layout.md) for the full design.

## Recommended Reading

- [architecture.md](architecture.md) for the overall storage model
- [admin-api.md](admin-api.md) for the admin endpoints
- [per-object-ec.md](per-object-ec.md) for current erasure-coding behavior
- [storage-layout.md](storage-layout.md) for volume identity and metadata architecture
