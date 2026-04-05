# Distributed Placement Testing

This document describes the current 4-node distributed placement test model in
AbixIO.

## Reference Topology

The reference test topology is fixed:

- 4 nodes
- 2 disks per node
- homogeneous disk capacity
- one logical set containing 8 total disks
- default cluster rule: if a node loses control-plane quorum, it fences and
  stops serving

The planner names every disk with a stable `(node_id, disk_id)` pair and stores
that identity inside shard metadata.

## Placement Rule

Placement is deterministic and node-first.

For an object `(bucket, key, EC profile)`:

1. Hash `epoch_id + set_id + bucket + key`.
2. Use that hash to derive a deterministic node order.
3. Place one shard on each node in that order.
4. Only after every node has one shard may a second disk on a node be used.

This gives better node fault isolation than a flat 8-disk shuffle.

Examples:

- `1+0`: one shard on one node
- `2+2`: four shards on four distinct nodes
- `4+4`: one shard on every disk

## What The Tests Prove

The planner unit tests prove:

- deterministic placement for the same key and epoch
- no duplicate disk assignment within one object
- spread across `min(total_shards, 4)` distinct nodes
- reasonable long-run distribution across nodes and disks
- rejection of invalid EC profiles

The 4-node integration tests prove:

- exact placement reported by `/_admin/object`
- every node reports the same placement map
- shard files exist only on the expected physical disks
- `2+2` objects survive one node loss
- fenced nodes return `503 ServiceUnavailable`

## What The Harness Simulates

The integration harness starts four S3 servers in-process.

Each server:

- exposes its own HTTP endpoint
- shares the same logical 8-disk placement topology
- sees node-owned disks through controlled backends that can be turned off for
  all servers at once

When a test stops a node, all disks owned by that node begin failing for every
server. This gives deterministic node-loss behavior without requiring a full
production RPC layer inside the test process.

## What This Is Not

This harness is not proof of production internode networking.

It does not yet validate:

- remote shard RPC
- dynamic topology rebalance
- consensus-driven topology commits
- heterogeneous set-class planning

Those remain future work on top of the current placement model and test suite.
