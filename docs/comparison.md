# Comparison

Reference page for where AbixIO overlaps with adjacent projects, where it is
actually different, and where the differentiation is still only planned.

## Disclaimer

This page is for orientation, not procurement.

Project status and capability notes for MinIO, Garage, RustFS, SeaweedFS, Ceph
Object Gateway, OpenStack Swift, OpenIO, and Riak CS are based on public GitHub
repositories, upstream docs, and public compatibility pages checked on
2026-04-05. These projects change over time. For any serious decision, verify
current behavior yourself.

Legend:

- `✅ Yes` = clearly supported / strong fit
- `⚠️ Partial` = supported in some form, but with limits or a different model
- `❌ No` = not supported or explicitly rejected
- `?` = not verified strongly enough to claim more

## Snapshot

| Project | One-line read | Overlap |
|---|---|---|
| **AbixIO** | Early Rust S3 server built to scale down cleanly to one node and up to multi-node clusters under one storage model. | Baseline |
| **MinIO** | Historical closest match: self-hosted S3 object storage server. | **High** |
| **Garage** | Closest current Rust neighbor in the self-hosted S3 space. | **Medium-high** |
| **RustFS** | Large, active Rust-native S3 object store positioned directly against MinIO and Ceph. | **Medium-high** |
| **SeaweedFS** | Real S3 competitor, but broader than object storage alone. | **Medium** |
| **Ceph Object Gateway** | Established object gateway in the Ceph ecosystem with S3 and Swift APIs. | **Medium** |
| **OpenStack Swift** | Long-running object store with native Swift API and optional S3 compatibility. | **Medium** |
| **OpenIO** | Object storage platform with an S3-compatible interface. | **Medium-low** |
| **Riak CS** | Older S3-compatible storage layer on top of Riak. | **Medium-low** |

## Project Status

| Project | Language | Public repo status | Public positioning |
|---|---|---|---|
| **AbixIO** | Rust | Active public repo. Very early stage. | S3-compatible erasure-coded object store for single-node or clustered deployment |
| **MinIO** | Go | **Archived** on GitHub. Upstream README says the repository is no longer maintained. | High-performance S3-compatible object store |
| **Garage** | Rust | Active public repo. README says it has been used in production since 2020. | S3-compatible distributed object storage for small-to-medium self-hosted geo-distributed deployments |
| **RustFS** | Rust | Active public repo, not archived, heavily starred and actively updated. | High-performance distributed object storage in Rust with S3 compatibility and coexistence / migration story for MinIO and Ceph |
| **SeaweedFS** | Go | Active public repo. | Distributed storage system with object storage, file system, WebDAV, Hadoop, cloud tiering, and more |
| **Ceph Object Gateway** | C++ / Ceph stack | Active project inside Ceph. | Object gateway exposing S3- and Swift-compatible APIs on top of Ceph |
| **OpenStack Swift** | Python | Active OpenStack project. | Distributed object storage with native Swift API and documented S3 compatibility information |
| **OpenIO** | Multiple / platform stack | Public docs and compatibility references exist; not one clean single-repo story here. | Object storage platform with S3-compatible interface |
| **Riak CS** | Erlang ecosystem | Legacy / older project docs remain available. | S3-compatible storage API layer for Riak |

## Overlap Map

| Rank | Project | Why the overlap is real |
|---|---|---|
| `1` | **MinIO** | Same simplest mental model: self-hosted S3-compatible object storage server. |
| `2` | **Garage** | Same broad product space, also Rust, also self-hosted distributed S3. |
| `3` | **RustFS** | Another Rust-native S3 object store with explicit competitive positioning against MinIO and Ceph. |
| `4` | **SeaweedFS** | Competes on S3 object storage use cases, but belongs to a broader storage platform category. |

## Rust-Native Peers

If the narrower question is "what else in Rust is trying to do this?", the
important names are:

| Project | Why it matters |
|---|---|
| **AbixIO** | Earlier and smaller, but differentiated by per-object EC, flexible single-node through multi-node topology, heterogeneous layouts, and a thick-console operating model. |
| **Garage** | Smaller-scale, geo-distributed, explicitly duplication-based Rust object store. |
| **RustFS** | Much larger public footprint, aggressive performance positioning, and direct S3 object-store ambitions in Rust. |

## Broader Ecosystem

These are adjacent systems worth knowing about even when they are not the
closest architectural match.

| Project | Why it matters in the comparison |
|---|---|
| **AbixIO** | Baseline for the comparison: an early Rust object store centered on explicit storage mechanics, flexible topology from one node upward, mixed layouts, and thick-client administration. |
| **Ceph Object Gateway** | One of the major established S3-compatible gateways in serious infrastructure environments. |
| **OpenStack Swift** | One of the oldest object-store ecosystems here, with explicit S3/Swift compatibility documentation. |
| **OpenIO** | Shows up in compatibility matrices and represents another object-storage implementation with an S3 surface. |
| **Riak CS** | Mostly historical context: a long-standing S3-compatible design showing what this space has already tried. |

## Core Capability Matrix

| Capability | AbixIO | MinIO | Garage | RustFS | SeaweedFS | Ceph RGW | Swift | OpenIO | Riak CS |
|---|---|---|---|---|---|---|---|---|---|
| Self-hosted multi-node storage | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes |
| S3-compatible API | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes | ⚠️ Partial | ✅ Yes | ✅ Yes |
| Object-store-first identity | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes | ⚠️ Partial | ⚠️ Partial | ✅ Yes | ✅ Yes | ✅ Yes |
| Rust implementation | ✅ Yes | ❌ No | ✅ Yes | ✅ Yes | ❌ No | ❌ No | ❌ No | ❌ No | ❌ No |
| Operational maturity | ⚠️ Partial: very early | ✅ Yes: very high historically | ✅ Yes: high | ✅ Yes: medium-high | ✅ Yes: high | ✅ Yes: high | ✅ Yes: high | ⚠️ Partial: medium | ⚠️ Partial: legacy |

## Storage Model Matrix

| Capability | AbixIO | MinIO | Garage | RustFS | SeaweedFS | Ceph RGW | Swift | OpenIO | Riak CS |
|---|---|---|---|---|---|---|---|---|---|
| **Single-node through multi-node under one operating model** | ✅ Yes: one node is a first-class deployment, and clustering scales out from the same storage model rather than a separate "real mode" product shape | ⚠️ Partial: single-node and clustered deployments exist, but the mental model is centered more on homogeneous server-pool layouts | ✅ Yes | ⚠️ Partial: supports single-node and clustered deployments, but pool constraints make the topology model more rigid | ✅ Yes | ⚠️ Partial | ⚠️ Partial | ⚠️ Partial | ⚠️ Partial |
| **Heterogeneous nodes / mixed disk layouts** | ✅ Yes: AbixIO's volume-pool design does not require identical node disk layouts, though live topology changes and rebalance are still future work | ⚠️ Partial: mixed hardware is feasible across server pools, but MinIO expects strong sameness within a pool/erasure-set layout | ✅ Yes: native multi-HDD support allows different per-drive capacities on a node and balances proportionally | ⚠️ Partial: RustFS scaling docs require identical disk type/capacity/quantity within a storage pool; heterogeneity is pool-to-pool, not node-to-node within a pool | ✅ Yes: SeaweedFS explicitly allows adding any server with disk space as more volume capacity | ✅ Yes: Ceph is built around weighted heterogeneous OSDs and CRUSH placement across mixed devices | ✅ Yes: Swift rings weight devices and support heterogeneous capacity layouts | ⚠️ Partial: OpenIO policy/placement model is flexible, but I did not verify a clean upstream statement specifically about mixed node disk layouts | ⚠️ Partial: Riak CS sits on Riak KV and does not present a first-class mixed-volume layout story in the checked docs |
| Erasure coding | ✅ Yes | ✅ Yes: parity is described at the erasure-set / deployment level | ❌ No: Garage design docs explicitly reject erasure coding and limit Garage to duplication | ✅ Yes: source and docs show an erasure-coded storage engine, erasure-set healing, and EC planning guidance | ✅ Yes: documented around erasure-coded volumes and warm storage | ⚠️ Partial: comes from the underlying Ceph storage stack rather than RGW as its own storage model | ✅ Yes: Swift documents erasure coding as a storage policy | ✅ Yes: OpenIO documents erasure-coding storage policies with configurable `k` and `m` | ⚠️ Partial: not central to checked docs used here; Riak CS docs emphasize chunking plus replication on Riak |
| **Per-object EC / parity selection** | ✅ Yes | ❌ No: checked MinIO EC docs describe erasure-set parity, not object-level parity selection | ❌ No: Garage explicitly avoids erasure coding | ❌ No verified user-facing per-object EC selector; checked docs point to EC planning and storage-class style configuration | ❌ No: checked SeaweedFS docs describe EC at volume / warm-storage level | ❌ No verified RGW-level per-object EC selector; placement/storage classes are bucket-centric in checked docs | ❌ No: Swift EC is attached to a storage policy chosen when the container is created | ⚠️ Partial: OpenIO docs state storage policy can be chosen at object, container, or namespace level, and EC `k/m` live in the selected policy | ❌ No verified per-object EC selector in checked docs |
| Self-describing volume identity on disk | ✅ Yes | ❌ No evidence in checked docs/source; MinIO docs focus on erasure sets and deployment topology | ❌ No evidence in checked docs/source; Garage public docs focus on cluster/distribution properties instead | ❌ No evidence in checked docs/source; checked docs emphasize JBOD disks and EC sets, not self-describing disk identity | ❌ No evidence in checked docs/source | ❌ No evidence in checked docs/source; RGW docs describe placement on Ceph, not self-describing gateway volumes | ❌ No: Swift uses externally managed rings to map data to devices | ❌ No evidence in checked docs/source | ❌ No evidence in checked docs/source; Riak CS docs focus on Riak backends beneath the API layer |
| Arbitrary pluggable volume backends | ⚠️ Partial: today through `Backend`; broader third-party backends are planned | ⚠️ Partial: official docs expose object tiering to remote MinIO, Azure, GCS, and S3, but not arbitrary interchangeable volume backends | ❌ No: checked Garage docs position it as duplicated storage nodes, not pluggable backend volumes | ⚠️ Partial: source includes warm-tier backends for S3, MinIO, Azure, GCS, R2, Aliyun, Tencent, and Huawei Cloud, but not as arbitrary interchangeable primary volume backends | ⚠️ Partial: Cloud Drive, remote object-store gateway, and cloud tiering are documented, but not as arbitrary interchangeable volume backends | ❌ No: gateway surface over Ceph, not arbitrary volume plugins | ⚠️ Partial: Swift has pluggable on-disk back-end APIs, but not a generic cloud-volume plugin model | ❌ No: checked docs describe storage/data-security policies and protocol connectors rather than arbitrary pluggable backend volumes | ⚠️ Partial: Riak KV beneath Riak CS supports multiple backends, but Riak CS itself is an S3 layer over that stack, not a generic volume-plugin model |

## Admin And Operations Matrix

| Capability | AbixIO | MinIO | Garage | RustFS | SeaweedFS | Ceph RGW | Swift | OpenIO | Riak CS |
|---|---|---|---|---|---|---|---|---|---|
| Admin tooling | ⚠️ Partial: current API/admin surface is strong, and the intended thick desktop console is part of the architecture, but the full multi-node console story is still emerging | ✅ Yes | ✅ Yes | ✅ Yes: README, console routes, admin handlers, Helm, and deployment assets all point to a substantial admin surface | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes: OpenIO docs describe REST-accessible admin and maintenance actions | ⚠️ Partial |
| **Thick multi-node admin/client console as a design goal** | ✅ Yes: AbixIO explicitly leans toward a native console that can aggregate and inspect multiple nodes directly instead of relying only on a per-node thin web UI | ❌ No: built-in web/admin tooling is the main model | ❌ No verified thick-console direction | ❌ No verified thick-console direction | ❌ No verified thick-console direction | ❌ No verified thick-console direction | ❌ No verified thick-console direction | ❌ No verified thick-console direction | ❌ No verified thick-console direction |
| Shard-level inspection | ✅ Yes | ⚠️ Partial: official docs expose `mc admin object info` for object shard summaries on disk | ❌ No specific shard-inspection workflow confirmed from checked docs/source | ⚠️ Partial: docs and source expose erasure-set/object inspection and healing information, but not the same shard-inspection workflow AbixIO exposes | ⚠️ Partial: maintenance and EC tooling exist, but not as the same shard-inspection model | ⚠️ Partial: admin APIs and tooling expose object/bucket state, but I did not verify an AbixIO-style shard inspection surface | ❌ No AbixIO-style shard inspection; Swift admin model is ring/object replication oriented | ⚠️ Partial: docs describe integrity loops, crawls, and REST maintenance actions over chunks/objects | ❌ No AbixIO-style shard inspection confirmed from checked docs |
| Healing / repair workflows | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes: docs and source include heal manager, erasure-set healer, scanner, resume logic, and bitrot handling | ✅ Yes | ✅ Yes | ✅ Yes: Swift documents replication, auditors, and EC repair flows | ✅ Yes: OpenIO docs explicitly describe self-healing / integrity loops and on-demand maintenance actions | ⚠️ Partial |

## Scope Matrix

| Capability | AbixIO | MinIO | Garage | RustFS | SeaweedFS | Ceph RGW | Swift | OpenIO | Riak CS |
|---|---|---|---|---|---|---|---|---|---|
| Narrow S3 server focus | ✅ Yes | ✅ Yes | ✅ Yes | ✅ Yes | ❌ No | ⚠️ Partial | ❌ No | ⚠️ Partial | ✅ Yes |
| Broader storage platform beyond S3 | ❌ No | ⚠️ Partial | ⚠️ Partial | ⚠️ Partial | ✅ Yes | ✅ Yes: via the wider Ceph platform | ⚠️ Partial: broader OpenStack object-storage ecosystem, but not an S3-first product | ⚠️ Partial | ⚠️ Partial |
| Cloud tiering / cloud-integrated storage story | ⚠️ Partial: planned direction only | ✅ Yes: object tiering is documented | ❌ No cloud tiering story confirmed from checked docs/source | ⚠️ Partial: source includes warm-tier backends for multiple cloud/object targets, and docs position RustFS for multi-cloud use, but the tiering story is less explicit than MinIO/SeaweedFS | ✅ Yes: cloud tiering, Cloud Drive, and remote object-store gateway are documented | ⚠️ Partial: replication / multisite / wider Ceph ecosystem, but not the same tiering story | ❌ No: not central to checked docs used here | ⚠️ Partial: OpenIO docs describe protocol connectors and policy-driven architecture, but not the same tiering story | ? Unclear |

## Where AbixIO Is Different

### Log-structured small object storage

Small objects (<= 64KB) are written as needles to append-only log segments
instead of individual files. Inspired by older log-structured storage
ideas: objects are written exactly once to the log, never overwritten, GC
reclaims dead space. The specific design choices (64KB threshold, one
segment per disk, hash index in RAM, no fsync) come from our own benchmark
measurements -- see [write-log.md](write-log.md) for the latency breakdown
that drove each one. This eliminates the filesystem metadata overhead
(mkdir + file create) that makes 4KB operations slow on every
file-per-object storage system.

MinIO and RustFS mitigate small-object overhead by inlining data into their
metadata files (one file instead of two). AbixIO goes further by eliminating
the per-object file entirely. A 4KB PUT on 4 disks creates 4 sequential
appends instead of 12 filesystem operations.

### Per-object erasure coding

This is the clearest current differentiator.

AbixIO allows the data/parity ratio to vary per object:

`per-object FTT > bucket FTT`

That is a materially different storage story from the usual server-level or
pool-level redundancy model.

### Self-describing clustered volumes

AbixIO stores cluster and volume identity on the volumes themselves. That gives
it a distinctive recovery and reconstruction story compared with the usual
"bring the control plane back first" mental model.

### Storage-engine-facing admin workflows

AbixIO treats shard inspection, healing state, and volume health as first-class
server concerns. That is closer to exposing the storage engine than merely
wrapping an S3 API with an admin surface.

### Embrace of heterogeneous nodes

AbixIO is intentionally friendlier to mixed layouts than systems that expect
identical nodes or rigid pool symmetry.

The design centers on a volume pool:

- nodes do not need identical disk counts
- volumes do not need to be presented as one uniform per-node shape
- the architecture is comfortable with mixed local and remote volumes

That does not mean the topology story is finished. Live topology changes and
rebalance are still future work. But the design intent is clearly in favor of
heterogeneity, not against it.

### One-node through many-node topology without changing product modes

AbixIO is designed to scale down to one node and scale out to multiple nodes
without turning clustered operation into a different product with a different
mental model.

That matters because:

- a single-node deployment is fully valid, not a crippled demo mode
- a small home deployment and a larger cluster still use the same core storage
  model
- node count is supposed to change the resilience envelope, not the basic
  product identity

That is different from systems whose language and operational shape are much
more centered on uniform server pools or heavier control-plane assumptions.

### Thick UI exposure of storage mechanics

AbixIO does not want storage mechanics hidden behind a thin generic object-store
console.

The intended direction is to expose the real storage engine through the admin
API and thick UI, not hide it:

- shard placement
- shard composition
- healing state
- volume health
- EC choices
- backend identity
- backend capacity
- failure visibility
- repair operations
- object storage layout details

The stronger product point is not just "desktop UI exists." It is that a thick
console is a better fit for a multi-node storage system:

- a built-in web UI is fine for one node or basic local administration
- once several nodes exist, a per-node web panel fragments visibility unless
  one node becomes a control-plane proxy for the others
- a thick console can talk to multiple nodes directly, aggregate cluster state,
  and expose storage-engine details without forcing all operator traffic through
  one server

That is a different philosophy from systems that keep the storage layer more
opaque behind a generic thin S3 administration surface.

### Backend abstraction as a possible long-term differentiator

Current state:

- local directory volumes exist
- remote AbixIO node volumes exist over internode RPC

### Single-node is a first-class deployment mode

AbixIO is not only for clustered deployments.

It is intended to run as:

- a single-node server on Windows, Linux, or macOS
- a multi-volume local server on one machine
- a multi-node clustered deployment

The tradeoff is simple:

- single-node mode is fully valid
- what you lose is node-level resilience, not product support
- local disk redundancy still depends on the erasure-coding layout you choose

Planned direction:

- Google Drive
- OneDrive
- WebDAV
- other third-party storage providers participating as volume backends

If that happens, it becomes a meaningful differentiator. Right now it is still a
roadmap claim, not a shipped advantage.

## Validation Notes

Specific upstream claims this page is anchored to:

- **MinIO**
  - upstream README says the GitHub repository is no longer maintained
  - erasure coding docs describe erasure sets, parity values such as `EC:4`,
    and writes distributed across an erasure set
  - official docs expose object tiering targets for remote MinIO, Azure, GCS,
    and S3

- **Garage**
  - `goals.md` explicitly lists storage optimizations such as erasure coding as
    a non-goal and says Garage limits itself to duplication
  - README positions Garage as a lightweight geo-distributed S3 object store
    used in production since 2020

- **RustFS**
  - README positions RustFS as a high-performance distributed object storage
    system built in Rust
  - README explicitly claims S3 compatibility and coexistence / migration with
    MinIO and Ceph
  - README exposes a large built-in feature/status matrix and strong direct
    performance positioning
  - source clearly includes ECStore, erasure-set healing, bitrot protection,
    console/admin surfaces, and warm-tier backends for several cloud targets
  - RustFS docs describe object inspection and auto-recovery with `k` data
    shards and `m` parity shards across `n=k+m` block devices
  - RustFS scaling docs say all nodes within a single storage pool must use
    identical disk type, capacity, and quantity

- **SeaweedFS**
  - README positions SeaweedFS as a broader storage system with object storage,
    file system, WebDAV, Hadoop, and cloud features
  - README describes erasure coding specifically for warm storage
  - README documents Cloud Drive, remote object-store gateway, and cloud-tier
    style integrations

- **Ceph Object Gateway**
  - official docs position RGW as an object gateway with S3 and Swift APIs
  - it should be thought of as part of the larger Ceph storage platform, not as
    a narrow S3-only server in the same way as AbixIO or MinIO

- **OpenStack Swift**
  - official docs include an S3/Swift REST API comparison matrix
  - Swift remains primarily a Swift-native object store with S3 compatibility as
    a compatibility surface rather than its sole identity
  - Swift documents erasure coding as a storage policy, storage-policy choice
    at container creation time, externally managed rings, and pluggable on-disk
    back-end APIs
  - Swift ring docs describe weighted devices, which is the core mechanism for
    handling heterogeneous capacity layouts

- **OpenIO**
  - OpenIO docs describe S3 and Swift compatibility, REST-accessible admin and
    maintenance actions, and self-healing / integrity loops
  - OpenIO docs describe erasure-coding storage policies with configurable
    `k/m` and allow the storage policy to be selected at object, container, or
    namespace level

- **Riak CS**
  - official docs describe a storage API compatible with the Amazon S3 REST API
  - included mostly as historical context and prior art in the S3-compatible
    object-store space

- **AbixIO**
  - the repo implements per-object EC headers
    `x-amz-meta-ec-ftt`
  - the repo uses a `Backend` trait with local and `RemoteVolume` backends
  - volumes carry `.abixio.sys/volume.json`
  - the repo and README support standalone single-node operation as well as
    clustered deployment; the difference is node resilience, not support status

## Where AbixIO Is Weak

| Weakness | Reality today |
|---|---|
| Production credibility | None yet |
| Benchmarks | [Published](benchmarks.md): fastest PUT at all sizes (4KB: 1618 vs MinIO 367, 1GB: 465 vs MinIO 356 MB/s). Fair test (all clients disk I/O): 1GB GET ~245 MB/s for all servers (disk-bound). mc GET shows network gap: AbixIO 352 vs MinIO 832. See [layer-optimization.md](layer-optimization.md) |
| Failure injection / long-haul testing | Not there yet |
| Packaging / deployment story | Not there yet |
| S3 surface completeness | Incomplete |
| Trust relative to established systems | Low |

## Honest Positioning

> AbixIO is an early Rust S3-compatible object storage server that overlaps most
> with MinIO, Garage, and RustFS, but currently differentiates with per-object
> erasure coding, a storage model that spans single-node through multi-node
> deployment, self-describing clustered volumes, shard-level admin workflows,
> an explicitly thick-console operating model, and a planned backend model that
> could treat arbitrary storage providers as volume backends.

## Sources

- [MinIO repo](https://github.com/minio/minio)
- [MinIO erasure coding docs](https://docs.min.io/community/minio-object-store/operations/concepts/erasure-coding.html)
- [Garage repo](https://github.com/deuxfleurs-org/garage)
- [Garage S3 compatibility page](https://garagehq.deuxfleurs.fr/documentation/reference-manual/s3-compatibility/)
- [Garage design goals](https://github.com/deuxfleurs-org/garage/blob/main/doc/book/design/goals.md)
- [Garage related work](https://github.com/deuxfleurs-org/garage/blob/main/doc/book/design/related-work.md)
- [RustFS repo](https://github.com/rustfs/rustfs)
- [RustFS healing docs](https://docs.rustfs.com/troubleshooting/healing.html)
- [RustFS installation docs](https://docs.rustfs.com/installation/linux/single-node-single-disk.html)
- [SeaweedFS repo](https://github.com/seaweedfs/seaweedfs)
- [RustFS availability and scaling](https://docs.rustfs.com/concepts/availability-and-resiliency.html)
- [Ceph Object Gateway S3 API docs](https://docs.ceph.com/en/latest/radosgw/s3/)
- [Ceph Object Gateway overview](https://docs.ceph.com/en/latest/radosgw/)
- [OpenStack Swift docs](https://docs.openstack.org/swift/latest/)
- [OpenStack S3/Swift comparison matrix](https://docs.openstack.org/swift/rocky/s3_compat.html)
- [Swift storage policies](https://docs.openstack.org/swift/latest/overview_policies.html)
- [Swift components and rings](https://docs.openstack.org/swift/2024.1/admin/objectstorage-components.html)
- [Swift on-disk back-end APIs](https://docs.openstack.org/swift/latest/development_ondisk_backends.html)
- [OpenIO data access](https://docs.openio.io/latest/source/arch-design/data_access.html)
- [OpenIO erasure coding configuration](https://docs.openio.io/latest/source/admin-guide/configuration_ec.html)
- [OpenIO storage policies](https://docs.openio.io/latest/source/admin-guide/configuration_storagepolicies.html)
- [OpenIO SDS core concepts](https://docs.openio.io/latest/source/arch-design/sds_concepts.html)
- [Riak CS S3 API docs](https://docs.riak.com/riak/cs/latest/references/apis/storage/s3/index.html)
- [Riak CS architecture](https://docs.riak.com/riak/cs/latest/tutorials/fast-track/what-is-riak-cs/index.html)
- [Riak CS configuration overview](https://docs.riak.com/riak/cs/latest/cookbooks/configuration/index.html)
