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

Current reality as of 2026-04-13:

- first commit: 2026-04-04
- no releases, packaging, or production deployments
- 362 tests passing (201 lib + 32 admin + 4 distributed + 125 S3 integration across 9 files)
- 41 of 72 S3 API operations implemented
- protocol layer powered by s3s (SigV4 chunked auth, smithy XML, presigned URLs)
- fully async storage layer (tokio)
- conditional requests (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since) implemented
- versioning response headers wired through s3s DTOs
- core storage engine works with real S3 clients, but important gaps remain

## What AbixIO Is Good At

| Area | What you get |
|---|---|
| **S3 API** | Standard object storage access for AWS CLI, rclone, boto3, MinIO client, and custom tools |
| **Erasure coding** | Per-object FTT (failures to tolerate) with bucket defaults |
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

- No production field time or failure-injection testing
- Bucket delete fails when versioned objects exist
- No encryption at rest
- No live topology changes or rebalance
- No consensus-backed control plane such as Raft

## Read Next

| Doc | What it covers |
|---|---|
| [Status](status.md) | Per-feature completion ratings and release blockers |
| [Architecture](architecture.md) | Design principles, cluster direction, project structure |
| [Write Path](write-path.md) | Canonical PUT path from client request to final resting place |
| [Cluster](cluster.md) | Node exchange, quorum model, fencing behavior |
| [Raft control plane](raft.md) | openraft design for consensus-backed membership, placement, bucket settings |
| [Storage Layout](storage-layout.md) | On-disk identity, metadata layout, reconstruction model |
| [Per-Object EC](per-object-ec.md) | Per-object FTT and bucket defaults |
| [Admin API](admin-api.md) | Status, disks, healing, inspect, and EC controls |
| [S3 Compliance](s3-compliance.md) | Operation-by-operation audit of current API coverage |
| [Comparison](comparison.md) | Overlap with MinIO, Garage, RustFS, SeaweedFS, Ceph, Swift, and others |
