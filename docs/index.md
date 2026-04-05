# AbixIO Server

**S3-compatible object storage server in Rust with erasure coding, clustered
volume pools, and shard-level operational visibility.**

AbixIO is designed to run in more than one mode:

- a single-node server on Windows, Linux, or macOS
- a multi-volume local server on one machine
- a multi-node clustered deployment

Single-node mode is fully supported. The tradeoff is simple: you lose
node-level resilience, not product support.

---

## Status

!!! warning "Early development"
    AbixIO is not production-ready yet.

Current reality as of 2026-04-05:

- first commit: 2026-04-04
- no releases, packaging, or production deployments
- 95 automated tests
- 41 of 72 S3 API operations implemented
- core storage engine works with real S3 clients, but important gaps remain

## What AbixIO Is Good At

| Area | What you get |
|---|---|
| **S3 API** | Standard object storage access for AWS CLI, rclone, boto3, MinIO client, and custom tools |
| **Erasure coding** | Per-object data/parity control with object, bucket, and server defaults |
| **Storage model** | Volume-pool design across local and remote volumes |
| **Topology** | Single-node through multi-node deployment under one storage model |
| **Clustering** | Node identity exchange, persisted membership, quorum tracking, and fencing |
| **Integrity** | Per-shard checksums, bitrot detection, background healing |
| **Operations** | Admin API plus a thick-console direction for cluster-wide visibility and shard inspection |

## What Works Today

- Erasure-coded object storage across local and remote volumes
- Internode shard RPC over HTTP with JWT authentication
- Multi-node cluster formation and identity exchange
- Quorum-aware fencing when nodes become unsafe
- Deterministic placement metadata in object shards
- Background healing with MRF queue and integrity scanner
- Object versioning, tagging, multipart upload, and presigned URLs
- Bucket policy and lifecycle configuration storage

## What Is Still Missing Or Rough

- No production field time, benchmarks, load testing, or failure-injection testing
- Object tagging returns errors in the latest build
- Chunked transfer encoding is not properly stripped in relay paths
- Bucket delete fails when versioned objects exist
- No encryption at rest
- No live topology changes or rebalance
- No consensus-backed control plane such as Raft

## Quick Start

```bash
cargo build --release

mkdir -p /tmp/abixio/{d1,d2,d3,d4}

./target/release/abixio \
  --volumes /tmp/abixio/d{1...4} \
  --no-auth
```

With 4 volumes, the server auto-computes `3+1` erasure coding, which tolerates
1 volume failure.

```bash
curl -X PUT http://localhost:10000/mybucket
curl -X PUT -d "hello world" http://localhost:10000/mybucket/hello.txt
curl http://localhost:10000/mybucket/hello.txt
```

## Read Next

| Doc | What it covers |
|---|---|
| [Architecture](architecture.md) | Design principles, cluster direction, project structure |
| [Cluster](cluster.md) | Node exchange, quorum model, fencing behavior |
| [Storage Layout](storage-layout.md) | On-disk identity, metadata layout, reconstruction model |
| [Per-Object EC](per-object-ec.md) | Object-level data/parity overrides and precedence |
| [Admin API](admin-api.md) | Status, disks, healing, inspect, and EC controls |
| [S3 Compliance](s3-compliance.md) | Operation-by-operation audit of current API coverage |
| [Comparison](comparison.md) | Overlap with MinIO, Garage, RustFS, SeaweedFS, Ceph, Swift, and others |

## Honest Positioning

AbixIO is not trying to win on maturity today. It is interesting because of:

- per-object erasure coding
- one-node through multi-node deployment under one product model
- self-describing clustered volumes
- shard-level operational visibility
- a thick native console model that fits multi-node administration better than a thin per-node web panel
- a backend model that can grow beyond local disks

If you need a production S3-compatible object store right now, you should still
compare it against mature alternatives before betting on it.
