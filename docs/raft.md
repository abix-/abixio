# Raft control plane

**Authoritative for:** the design of a Raft-backed control plane for
abixio, using the `openraft` library. Covers scope, FSM shape,
storage layout, transport, admin API, bootstrap flow, fence
semantics, and migration from the current probe-based cluster
manager.

**Not covered here:** current probe-based clustering (see
[cluster.md](cluster.md)), object-shard placement planning (see
[cluster.md](cluster.md) and [per-object-ec.md](per-object-ec.md)),
data-plane durability (that is erasure coding, not Raft).

This document is the design; the implementation plan lives alongside
it and is rolled into landings by task. Status tracked in
[status.md](status.md) under the Clustering section.

## Summary

Raft replicates **state about the cluster**. Object shards never
enter the Raft log. The finite state machine holds cluster
membership, the placement topology (epoch + volume_ids), and bucket
settings (versioning, FTT, policy, lifecycle, tags). Everything else
stays where it is today.

We use the term **primary** for the node that holds the Raft
leadership; **secondary** for other voters; **observer** for
non-voting replicas (future).

## Why Raft

The current probe-based quorum model ([cluster.md](cluster.md)) is
intentional-minimum and works for a research prototype. It falls
short for four concrete feature directions:

1. **Per-object FTT.** Each object may carry its own erasure-coding
   parameters. The placement table that maps shard slots to volume
   ids at a given epoch must be totally ordered across every node;
   otherwise two nodes under partition can commit conflicting
   placements for the same key at different FTT settings. That is a
   data-integrity bug, not a liveness one.
2. **Singleton tasks.** Lifecycle enforcement, the integrity
   scanner, and the MRF drain coordinator currently run on every
   node. In an N-node cluster that is N-fold waste and N-fold risk
   of duplicate DELETEs. A primary runs them; secondaries nap.
3. **Atomic bucket config.** `PutBucketPolicy` on node A must be
   immediately visible on node B for the next request that lands on
   B. Today the per-disk meta.json rewrites have no cross-node sync
   cadence. Raft commits make the new policy active on every node as
   soon as a majority replicates the log entry.
4. **Operator visibility.** `/_admin/cluster/status` today is
   "according to this node". Two nodes can disagree. With Raft we
   return the canonical committed state.

These are not hypothetical; they shape the next two quarters of
feature work. Raft is the simplest protocol that closes all four
gaps.

## Why openraft

We surveyed the Rust Raft ecosystem. The two real choices are
`openraft` (databendlabs, async, trait-based) and `raft-rs` (tikv,
sync, core-only). We chose **openraft** for these reasons:

- async/tokio-native, dropping into our stack without impedance
- trait-based scaffolding (`RaftLogStorage`, `RaftStateMachine`,
  `RaftNetwork`) — less glue than raft-rs
- production usage at Databend, CnosDB, RobustMQ, and **Helyim** (a
  Rust SeaweedFS-style object store — our exact peer group)
- actively released in 2026

Known caveats:

- openraft is pre-1.0; upgrades may require migration work
- joint-consensus membership changes only (the correct design)
- upstream chaos testing is incomplete — we add our own fault
  injection

We do **not** use raft-rs because it is sync and ships only the core
consensus module. Wrapping it would be 2-3x the glue code for a
battle-testing we do not need at our scale.

## What Raft stores

The FSM keeps three tables in memory, persisted as snapshots:

### 1. Cluster membership

| Field | Type | Meaning |
|---|---|---|
| `node_id` | `String` | UUID generated at first boot |
| `advertise_s3` | `String` | Public S3 endpoint, e.g. `http://node-a:10000` |
| `advertise_cluster` | `String` | Internode endpoint (often same as above) |
| `voter_kind` | `Voter | Observer` | Voter participates in quorum; Observer is read-only (future) |
| `added_at` | `u64` | Unix-nanos when this node joined |

Replaces the runtime `--nodes` list. Topology changes go through
`/_admin/raft/join` + `/_admin/raft/leave`.

### 2. Placement topology

| Field | Type | Meaning |
|---|---|---|
| `epoch_id` | `u64` | Monotonic per-cluster counter |
| `volumes` | `Vec<PlacementVolume>` | Full ordered list of (backend_index, node_id, volume_id) |

Replaces `VolumePool::placement` (currently an in-RAM `RwLock`).
Advancing the epoch requires a Raft commit.

### 3. Bucket settings

| Field | Type | Meaning |
|---|---|---|
| `versioning` | `Option<String>` | `Enabled` / `Suspended` / absent |
| `ftt` | `Option<usize>` | Per-bucket FTT override |
| `tags` | `HashMap<String, String>` | Bucket tags |
| `policy` | `Option<Value>` | IAM-style JSON policy doc |
| `lifecycle` | `Option<String>` | JSON-encoded rule list |

Replaces the per-disk meta.json scatter writes for these fields.
Per-disk meta.json remains as a write-through recovery path so a
Raft log loss still leaves enough on-disk state to rebuild.

## What Raft does NOT store

- **Object shards.** Data plane is erasure coded to disks; the Raft
  log would grow without bound.
- **Object metadata (`meta.json`).** Per-object metadata stays on
  disk, discoverable by listing.
- **Multipart upload parts.** Staged in `.abixio.sys/multipart/`
  exactly as today.
- **Heal state (per-shard).** Per-disk integrity scanner output stays
  local. Whole-cluster scrub coordination is a follow-up that could
  use Raft but does not in this landing.
- **WAL state.** Per-disk write-ahead log stays per-disk.

Anything that is "data" is not in Raft; anything that is
"how the cluster is configured" goes into Raft.

## Terminology

User-facing code, docs, and CLI use:

- **primary** — the current Raft leader; accepts writes to cluster
  state
- **secondary** — other voters, replicating the log
- **observer** — non-voting read replica (future)

Never "leader" or "master" in user-facing surfaces.
`openraft::RaftMetrics.current_leader` exists internally and we read
from it; we just do not print it verbatim.

## Architecture

### FSM

`src/raft/fsm/` holds the state machine and its operations.

- `fsm/mod.rs` — `AbixioStateMachine` implementing
  `openraft::RaftStateMachine`. In-memory `RwLock` over the three
  tables plus an applied-index cursor. Snapshot serialization is
  msgpack; restore replaces the state atomically.
- `fsm/ops.rs` — `Op` enum:
  - `AddNode { node_id, advertise_s3, advertise_cluster, voter_kind }`
  - `RemoveNode { node_id }`
  - `SetPlacementEpoch { epoch_id, volumes }`
  - `SetBucketSettings { bucket, settings }`
  - `DeleteBucketSettings { bucket }`
- `fsm/query.rs` — pure read helpers: `bucket_settings(bucket)`,
  `placement_snapshot()`, `member(node_id)`. Reads take a shared
  lock; no Raft round trip unless linearizability is requested.

Linearizable reads use `openraft::Raft::ensure_linearizable` (issues
a read-index through the primary). Ordinary reads go straight to the
local FSM.

### Storage

Each node persists its own Raft state under:

```
<first local disk>/.abixio.sys/raft/
├── log.dat          # append-only, mmap; grows until snapshot
├── snap-<idx>.msgpack
├── snap-<idx-1>.msgpack   # previous snapshot kept for one cycle
└── vote.json        # current term + voted-for, fsynced on update
```

- `storage/log.rs` — `RaftLogStore` implementing
  `openraft::RaftLogStorage`. Mirrors the segment mmap pattern in
  `src/storage/segment.rs`. Truncates on snapshot install.
- `storage/snapshot.rs` — snapshot read/write. Atomic rename. Keep
  the last two snapshots to tolerate mid-install crashes.
- `storage/meta.rs` — `HardState` (term + voted-for) as a small JSON
  file, fsynced on update.

The first local disk is chosen to avoid requiring Raft state on
every disk. If the first disk dies, the node is restarted with a
different first-disk designation and rejoins via snapshot install
from a peer.

### Transport

Raft messages use the existing internode channel
(`/_storage/v1/*`), adding four routes:

- `POST /_storage/v1/raft/append-entries`
- `POST /_storage/v1/raft/install-snapshot`
- `POST /_storage/v1/raft/vote`
- `POST /_storage/v1/raft/pre-vote`

JWT auth continues via `internode_auth`. Message bodies are bincode.
`src/raft/network.rs` implements `openraft::RaftNetwork` as a
reqwest client, reusing the shape from `src/storage/peer_cache.rs`.

No new listener. No new port.

### Admin API

Public operator surface under `/_admin/raft/*`:

| Endpoint | Method | Meaning |
|---|---|---|
| `/_admin/raft/peers` | GET | List all members with role, last log index, time since last contact |
| `/_admin/raft/primary` | GET | `{node_id, advertise_s3}` of current primary |
| `/_admin/raft/bootstrap` | POST | Initial single-node bootstrap (409 if already bootstrapped) |
| `/_admin/raft/join` | POST | This node asks the primary to add it as a voter |
| `/_admin/raft/leave` | POST | This node asks the primary to remove it |
| `/_admin/raft/snapshot` | POST | Manual snapshot trigger for ops/testing |

`/_admin/cluster/status` continues to work and now pulls canonical
membership + primary from Raft, with reachability from the existing
probe loop.

### Singleton dispatcher

`src/raft/singleton.rs` exposes `is_primary()` by reading
`openraft::RaftMetrics.state`. Existing background loops become:

- `lifecycle_loop` — skip tick when `!is_primary()`
- `scanner_loop` — skip tick when `!is_primary()`
- `mrf_drain_worker` — per-disk drain stays on every node (heal is
  per-disk); whole-cluster scrub scheduling becomes primary-only in
  a follow-up

Primary election changes flip the gate on the next tick; no
coordination needed.

## Bootstrap and topology changes

### First node

```bash
abixio --volumes /data/d{0...3} --listen :10000 --raft-bootstrap
```

`--raft-bootstrap` tells this node to call
`openraft::Raft::initialize` with itself as the only voter. Returns
409 if the Raft log shows we are already bootstrapped.

### Subsequent nodes

```bash
abixio --volumes /data/d{0...3} --listen :10000 \
  --nodes http://node-a:10000
```

On first boot:

1. Node B starts with empty Raft state.
2. It discovers `node-a` via `--nodes`.
3. It calls `POST http://node-a:10000/_admin/raft/join` with its
   own `node_id` + `advertise_s3`.
4. Primary accepts, emits an `AddNode` log entry, waits for commit.
5. Primary sends Node B a snapshot install.
6. Node B applies and becomes a voting secondary.

### Leave

`POST /_admin/raft/leave` on the node being removed. Primary emits a
`RemoveNode` log entry. Once committed, the node shuts down its own
Raft instance.

### Recovery (primary crash)

openraft handles leader election automatically. Secondaries run
`pre-vote` then `vote` once the heartbeat timeout expires; the one
with the most up-to-date log wins. Election completes in one to two
heartbeat timeouts (default ~300ms).

## Fence behavior

Fencing now reads Raft state:

- If Raft reports this node is not in the voter set, fence the data
  plane — return `503 ServiceUnavailable` on S3 writes and mutating
  admin requests.
- If Raft has no primary for longer than a configurable timeout
  (default 10s), fence mutating operations but keep serving reads
  from the local FSM + disk.
- The existing probe loop stays alive for reachability detection
  during partitions; its output feeds `/_admin/cluster/status` but
  no longer drives the fence.

## Wiring into VolumePool + bucket settings

`Store::get_bucket_settings` / `set_bucket_settings` route through
Raft:

- `get_bucket_settings(bucket)` — read from local FSM under a
  shared lock. No round trip. On a secondary this is ≤1μs.
- `set_bucket_settings(bucket, new)` — if `is_primary()`, submit
  `SetBucketSettings` op and await commit (single LAN round trip to
  majority). If secondary, HTTP-forward to the primary using the
  existing `_storage/v1/*` channel.
- Per-disk meta.json stays as a write-through recovery path. Reads
  prefer the FSM and fall back to disk only when the FSM has no
  record (cold-start, Raft log loss).

## Migration path

1. Ship openraft behind a feature gate; `ClusterManager` keeps its
   probe loop and remains source of truth for a release.
2. Introduce FSM reads on the non-critical path (e.g. `/_admin/cluster/status`)
   to validate.
3. Migrate `bucket_settings` reads to the FSM with disk fallback.
4. Migrate `bucket_settings` writes through Raft.
5. Retire the old lowest-node-id "leader pick" in favor of
   `is_primary()`.
6. Document openraft as the canonical control-plane in
   [cluster.md](cluster.md), flip the "no consensus" claim.

## Caveats we accept

- **openraft pre-1.0.** We pin a minor version and plan for a major
  upgrade before we cut v1.0.
- **Joint consensus only for membership.** Adding two voters at
  once requires two serial adds. Acceptable.
- **Chaos testing.** openraft upstream docs note chaos testing is
  incomplete. We add our own fault injection under
  `tests/raft_cluster.rs`: partitions, clock skew, primary kill,
  secondary lag, disk errors on the log store.
- **Per-node Raft log on first disk only.** If that disk dies, the
  node rejoins via snapshot install. Operators must recognize the
  first-disk designation matters.

## Out of scope

- Object data or metadata in the Raft log
- Multi-raft / sharded consensus groups (single group until profiled
  to the bottleneck)
- Automatic rebalance on topology change
- Observer / non-voting read replicas (voter_kind enum is defined
  but only `Voter` is instantiated)
- Graceful leader transfer on drain
- Vault-style autopilot (stabilization window, dead-server cleanup,
  redundancy zones) — separate landing after openraft is live
- Snapshot compression or incremental snapshots — full-state msgpack
  is fine until the FSM grows past a few MB

## Open questions (to resolve during implementation)

1. **Linearizable reads for policy evaluation.** `AbixioAccess::check`
   fetches bucket policy on every request. Should it take a
   linearizable read (one LAN RTT per request on secondaries) or a
   local-consistent read (stale by up to one commit)? Probable
   answer: local-consistent is correct for a storage server;
   security policies propagate fast enough that one-commit staleness
   is a non-issue.
2. **Per-object FTT write latency.** Placement epoch advances per
   FTT change — expected to be rare. But if per-object FTT means
   the placement table grows unbounded over time, we may want to
   snapshot the FSM on a time schedule as well as a size threshold.
3. **Raft log location on all-new-disk installs.** First-disk
   semantics are clear on reboot, but on a fresh multi-disk install
   should we always pick `volumes[0]`, or hash the node_id? First
   option is easier to reason about.

## References

- [openraft (databendlabs)](https://github.com/databendlabs/openraft)
- [openraft docs](https://databendlabs.github.io/openraft/)
- Helyim — Rust SeaweedFS using openraft in production
- [cluster.md](cluster.md) — the current probe-based clustering
- [per-object-ec.md](per-object-ec.md) — the placement shape Raft
  will own
- [status.md](status.md) — tracking score changes as landings ship

## Accuracy Report

This document describes a design. No code has landed yet. Once the
first implementation lands, add the usual accuracy report here with
file:line evidence for each claim.
