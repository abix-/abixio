# Comparison

Reference page for where AbixIO overlaps with adjacent projects, where it is
actually different, and where the differentiation is still only planned.

## Disclaimer

This page is for orientation, not procurement.

Project status and capability notes for MinIO, Garage, and SeaweedFS are based
on their public GitHub repositories, top-level READMEs, and public docs as
checked on 2026-04-05. These projects change over time. For any serious
decision, verify current behavior yourself.

`Strong` means the capability is clearly present or central to the project's
public story.  
`Partial` means it exists in some form, but not in the same shape or not as a
primary feature.  
`No public indication` means it was not found in the public sources used for
this page.

## Snapshot

| Project | One-line read | Overlap |
|---|---|---|
| **MinIO** | Historical closest match: self-hosted S3 object storage server. | **High** |
| **Garage** | Closest current Rust neighbor in the self-hosted S3 space. | **Medium-high** |
| **SeaweedFS** | Real S3 competitor, but broader than object storage alone. | **Medium** |
| **AbixIO** | Early Rust S3 server with unusual storage-engine choices. | Baseline |

## Project Status

| Project | Language | Public repo status | Public positioning |
|---|---|---|---|
| **MinIO** | Go | **Archived** on GitHub. Upstream README says the repository is no longer maintained. | High-performance S3-compatible object store |
| **Garage** | Rust | Active public repo. README says it has been used in production since 2020. | S3-compatible distributed object storage for small-to-medium self-hosted geo-distributed deployments |
| **SeaweedFS** | Go | Active public repo. | Distributed storage system with object storage, file system, WebDAV, Hadoop, cloud tiering, and more |
| **AbixIO** | Rust | Active public repo. Very early stage. | S3-compatible erasure-coded object store |

## Overlap Map

| Rank | Project | Why the overlap is real |
|---|---|---|
| `1` | **MinIO** | Same simplest mental model: self-hosted S3-compatible object storage server. |
| `2` | **Garage** | Same broad product space, also Rust, also self-hosted distributed S3. |
| `3` | **SeaweedFS** | Competes on S3 object storage use cases, but belongs to a broader storage platform category. |

## Core Capability Matrix

| Capability | MinIO | Garage | SeaweedFS | AbixIO |
|---|---|---|---|---|
| Self-hosted multi-node storage | Strong | Strong | Strong | Strong |
| S3-compatible API | Strong | Strong | Strong | Strong |
| Object-store-first identity | Strong | Strong | Partial | Strong |
| Rust implementation | No | Strong | No | Strong |
| Operational maturity | Very high historically | High | High | Very early |

## Storage Model Matrix

| Capability | MinIO | Garage | SeaweedFS | AbixIO |
|---|---|---|---|---|
| Erasure coding | Strong: server/pool level | No public indication; Garage design docs emphasize duplication instead | Strong: presented around erasure-coded volumes and warm storage | Strong |
| **Per-object EC / parity selection** | No | No public indication | No public indication | **Strong** |
| Self-describing volume identity on disk | No public indication | No public indication | No public indication | Strong |
| Deterministic shard placement metadata stored with object | No public indication | No public indication | No public indication | Strong |
| Arbitrary pluggable volume backends | No public indication | No public indication | Partial: cloud/tiering integrations, but not presented as interchangeable volume backends | Partial today through `Backend`; broader third-party backends are planned |

## Admin And Operations Matrix

| Capability | MinIO | Garage | SeaweedFS | AbixIO |
|---|---|---|---|---|
| Admin tooling | Strong | Strong | Strong | Partial |
| Shard-level inspection | Partial: mature tooling, different model | No public indication | Partial: maintenance and admin tooling, different architecture | Strong |
| Healing / repair workflows | Strong | Strong | Strong | Strong |
| Cluster fencing / fail-closed behavior as explicit public story | No public indication | No public indication | No public indication | Strong |

## Scope Matrix

| Capability | MinIO | Garage | SeaweedFS | AbixIO |
|---|---|---|---|---|
| Narrow S3 server focus | Strong | Strong | No | Strong |
| Broader storage platform beyond S3 | Partial | Partial | **Strong** | No |
| Cloud tiering / cloud-integrated storage story | Partial | No public indication | Strong | Planned direction only |

## Where AbixIO Is Different

### Per-object erasure coding

This is the clearest current differentiator.

AbixIO allows the data/parity ratio to vary per object:

`object header > bucket config > server default`

That is a materially different storage story from the usual server-level or
pool-level redundancy model.

### Self-describing clustered volumes

AbixIO stores cluster and volume identity on the volumes themselves. That gives
it a distinctive recovery and reconstruction story compared with the usual
\"bring the control plane back first\" mental model.

### Storage-engine-facing admin workflows

AbixIO treats shard inspection, healing state, and volume health as first-class
server concerns. That is closer to exposing the storage engine than merely
wrapping an S3 API with an admin surface.

### Backend abstraction as a possible long-term differentiator

Current state:

- local directory volumes exist
- remote AbixIO node volumes exist over internode RPC

Planned direction:

- Google Drive
- OneDrive
- WebDAV
- other third-party storage providers participating as volume backends

If that happens, it becomes a meaningful differentiator. Right now it is still a
roadmap claim, not a shipped advantage.

## Where AbixIO Is Weak

| Weakness | Reality today |
|---|---|
| Production credibility | None yet |
| Benchmarks | None published |
| Failure injection / long-haul testing | Not there yet |
| Packaging / deployment story | Not there yet |
| S3 surface completeness | Incomplete |
| Trust relative to established systems | Low |

## Honest Positioning

> AbixIO is an early Rust S3-compatible object storage server that overlaps most
> with MinIO and Garage, but currently differentiates with per-object erasure
> coding, self-describing clustered volumes, shard-level admin workflows, and a
> planned backend model that could treat arbitrary storage providers as volume
> backends.

## Sources

- [MinIO repo](https://github.com/minio/minio)
- [Garage repo](https://github.com/deuxfleurs-org/garage)
- [Garage S3 compatibility page](https://garagehq.deuxfleurs.fr/documentation/reference-manual/s3-compatibility/)
- [SeaweedFS repo](https://github.com/seaweedfs/seaweedfs)
