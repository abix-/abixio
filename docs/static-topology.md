# Static Topology

AbixIO's current cluster model is a deliberately small control plane.

Instead of self-registration, live membership changes, or a consensus log, all
nodes start from the same topology manifest. That manifest is the authoritative
cluster description for:

- `cluster_id`
- `epoch_id`
- `set_id`
- node identities
- advertised S3 and cluster endpoints
- disk identities and disk ownership

## Why This Model Exists

The current goal is to keep multi-node behavior minimal and predictable:

- deterministic object placement
- startup-time topology validation
- strict fencing on unsafe cluster state
- explicit restart-based reconfiguration

This avoids building Raft or a dynamic control plane before the distributed
data plane exists.

## Manifest Schema

The topology file is JSON.

```json
{
  "cluster_id": "cluster-a",
  "epoch_id": 1,
  "set_id": "set-a",
  "nodes": [
    {
      "node_id": "node-1",
      "advertise_s3": "http://node-1.lan:9000",
      "advertise_cluster": "http://node-1.lan:9000",
      "disks": [
        { "disk_id": "disk-1a", "path": "/srv/abixio/node-1/d1" },
        { "disk_id": "disk-1b", "path": "/srv/abixio/node-1/d2" }
      ]
    },
    {
      "node_id": "node-2",
      "advertise_s3": "http://node-2.lan:9000",
      "advertise_cluster": "http://node-2.lan:9000",
      "disks": [
        { "disk_id": "disk-2a", "path": "/srv/abixio/node-2/d1" },
        { "disk_id": "disk-2b", "path": "/srv/abixio/node-2/d2" }
      ]
    }
  ]
}
```

## Validation Rules

The server rejects a topology file if any of these are true:

- `cluster_id` is empty
- `epoch_id` is `0`
- `set_id` is empty
- no nodes are defined
- any `node_id` is duplicated
- any `disk_id` is duplicated
- any disk path is duplicated anywhere in the manifest
- a node has no disks

At node startup, AbixIO also validates local identity:

- `--node-id` must match a manifest node
- local `--disks` must exactly match that node's disk paths

This is an exact set comparison after path normalization. A node does not start
if its local disk inventory disagrees with the manifest.

## Startup Semantics

Start each node with the same manifest file:

```bash
./target/release/abixio \
  --listen 0.0.0.0:9000 \
  --node-id node-1 \
  --cluster-topology /etc/abixio/cluster.json \
  --disks /srv/abixio/node-1/d1,/srv/abixio/node-1/d2 \
  --data 2 --parity 2
```

When `--cluster-topology` is set, the manifest becomes authoritative for:

- `cluster_id`
- current `epoch_id`
- peer endpoints
- `advertise_s3`
- `advertise_cluster`

The local node still serves the same S3 and admin APIs, but the cluster manager
derives cluster identity and peer relationships from the manifest instead of
from ad hoc flags.

The manifest's `set_id` is also used by the placement layer when the server can
install the full placement topology into the local `ErasureSet`.

## Topology Hash

The cluster summary exposes a `topology_hash`. This is a SHA-256 hash of the
serialized topology manifest.

Its purpose is operational:

- confirm every node loaded the same manifest
- detect accidental drift
- make admin status easier to compare across nodes

This is not a consensus proof. It only tells you what the local node believes
the topology file is.

## Placement Interaction

The placement planner uses stable placement metadata:

- `epoch_id`
- `set_id`
- ordered `node_ids`
- ordered `disk_ids`

That placement identity is stored in shard metadata for each object when the
placement-aware path is active. Admin inspection returns it through
`/_admin/object`.

Current limitations:

- if the manifest describes more disks than the local node has attached, the
  server keeps the manifest for identity and validation, but the local
  `ErasureSet` does not automatically switch to full cluster-wide placement
  because production remote shard transport is not implemented yet
- in a real multi-node deployment today, the manifest is authoritative for
  cluster identity, epoch, peer relationships, and validation, but not yet for
  production cross-node shard writes

The 4-node distributed placement test harness simulates the full logical disk
set on one machine, which is why the planner and placement tests are already
present even though network shard RPC is still future work.

## Reconfiguration

Reconfiguration is intentionally explicit and restart-based in the current
design.

To change topology:

1. prepare a new manifest with `epoch_id + 1`
2. distribute it to every node
3. stop the cluster
4. restart all nodes with the new manifest
5. confirm the new `topology_hash` and `epoch_id` on every node

What this does today:

- the cluster manager loads the new cluster identity and epoch
- old objects keep their stored placement identity
- new writes only use the new full placement identity when the placement-aware
  path is active, which currently requires the full logical disk set to be
  locally available as in the test harness

What this does not do yet:

- live rolling membership changes
- rebalance
- migration of old objects to the new topology
- consensus-committed epoch activation

## What This Model Intentionally Excludes

This v1 model does not include:

- Raft
- etcd
- live self-registration
- leader-driven dynamic topology planning
- heterogeneous set-class planning
- production internode shard RPC

If AbixIO later needs online cluster changes, live rebalance, or autonomous
membership, it will need a stronger control plane than this manifest model.
