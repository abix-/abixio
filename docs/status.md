# Project status

Completion ratings for every area that matters before AbixIO can be
called production-ready. Rated 1-10 where 1 = concept only, 5 = works
but gaps remain, 10 = production-grade.

Last updated: 2026-04-16

## Overall: 4/10

AbixIO is a working S3-compatible storage server with strong internals
and real benchmarks, but it has no releases, no packaging, no field
time, and several missing safety features. It is a research project,
not a product.

---

## Core storage engine: 7/10

| Area | Rating | Notes |
|---|---|---|
| Erasure coding (RS encode/decode) | 8/10 | per-object FTT, streaming encode, zero-alloc decode, bitrot detection |
| Write path (file tier) | 8/10 | direct mkdir+write |
| Write path (WAL) | 8/10 | append to mmap, background materialize, 3us at 4KB in release mode. carries version_id. no permanent index |
| Write cache (RAM) | 7/10 | DashMap, primary-tagged entries, background flush task, single-peer synchronous replicate-before-ack, startup pull from peers. no eviction policy (cache full -> disk write) |
| Read cache (RAM) | 7/10 | LRU bounded by total bytes, per-object size ceiling. Invalidated on PUT/DELETE/versioning ops. No frequency tracking yet |
| Read path (GET) | 8/10 | mmap zero-copy for 1+0, zero-alloc mmap slices for EC, range support |
| Streaming large objects | 7/10 | unified ShardWriter encode path, no full-body buffering. streaming GET works |
| Volume placement | 7/10 | deterministic placement, quorum writes, multi-disk spread. no rebalance |
| Metadata / on-disk format | 7/10 | self-describing volumes, meta.json per object, volume.json identity. format version validated on load (hard-fails unknown versions). needle/segment format version still implicit |

## S3 API coverage: 5/10

41 of 72 operations implemented (57%).

| Area | Rating | Notes |
|---|---|---|
| Object CRUD (PUT/GET/HEAD/DELETE) | 9/10 | conditionals, versioning, per-object EC, range, metadata |
| Object listing | 7/10 | v2, versions, prefix/delimiter. no v1 |
| Multipart upload | 8/10 | create, upload part, complete, abort, list parts. no CopyObjectPart |
| Bucket operations | 7/10 | create, delete, head, list, versioning, tagging, policy, lifecycle config |
| Bucket policy enforcement | 6/10 | Allow/Deny statements evaluated per request; supports `Principal: "*"`/AWS list + action + resource globs. Conditions, NotAction/NotResource, IAM roles not enforced |
| Lifecycle enforcement | 6/10 | background engine applies `Expiration.days`, `ExpiredObjectDeleteMarker`, `NoncurrentVersionExpiration`, `AbortIncompleteMultipartUpload`, `Filter.prefix`. `Expiration.date`, tiering transitions, tag/size filters, `DelMarkerExpiration` not enforced |
| Encryption config | 0/10 | not implemented |
| Object lock / retention | 0/10 | not implemented |
| Replication | 0/10 | not implemented |
| ACLs | 2/10 | stubs return hardcoded FULL_CONTROL, never enforced |
| CORS | 2/10 | stubs only |
| Notifications | 2/10 | stubs only |

## Auth and security: 7/10

| Area | Rating | Notes |
|---|---|---|
| SigV4 header auth | 9/10 | handled by s3s, well-tested |
| SigV4 presigned URLs | 9/10 | handled by s3s |
| SigV4 chunked transfer | 9/10 | handled by s3s |
| Content-MD5 validation | 9/10 | handled by s3s |
| Anonymous / no-auth mode | 8/10 | works |
| Path traversal hardening | 8/10 | bucket/key/version_id validated, regression tests in place |
| Internode JWT auth | 7/10 | signed with S3 credentials, validates on receive |
| Encryption at rest | 0/10 | design doc only, no implementation |
| TLS | 7/10 | works with rustls, cert/key from CLI flags. no auto-cert |

## Clustering: 5/10

| Area | Rating | Notes |
|---|---|---|
| Node identity exchange | 7/10 | automatic at startup via --nodes |
| Self-describing volumes | 8/10 | volume.json on every volume, cluster reconstructable from disks |
| Internode shard RPC | 7/10 | RemoteVolume over HTTP, JWT auth |
| Internode cache replication | 7/10 | cache-replicate + cache-sync endpoints, JWT auth, msgpack body. single-peer replica before ack. N-way deferred |
| Quorum tracking | 6/10 | probe-based, fences on quorum loss |
| Hard fencing | 7/10 | rejects S3 and mutating admin requests when unsafe |
| Placement metadata | 7/10 | epoch_id + volume_ids in every shard |
| Live topology changes | 0/10 | nodes fixed at startup |
| Rebalance | 0/10 | not implemented |
| Consensus (Raft etc.) | 0/10 | not implemented |

## Healing and integrity: 6/10

| Area | Rating | Notes |
|---|---|---|
| MRF (reactive heal) | 7/10 | bounded dedup queue, drain worker, triggers on partial write failure |
| Integrity scanner | 6/10 | walks all objects, verifies checksums, per-object EC aware. single worker |
| Per-shard checksums | 8/10 | blake3, SIMD-accelerated, checked on read |
| WAL heal awareness | 8/10 | scanner + heal share VolumePool's WAL-enabled backends via `heal_backends()`, so `read_shard` / `stat_object` / `list_objects` see un-materialized entries. No peer-divergence detection inside WAL itself (EC quorum still covers durability) |
| Failure injection tests | 7/10 | 12 tests covering shard corruption, missing shards, streaming GET bitrot. no partition / slow-disk injection |

## Testing: 6/10

| Area | Rating | Notes |
|---|---|---|
| Unit tests (lib) | 7/10 | 182 tests in src/. covers core paths |
| Integration tests | 7/10 | 176 tests in tests/. S3 ops, admin, distributed placement, cache replication, failure injection |
| Benchmark suite | 8/10 | L1-L7 layered benchmarks, competitive matrix, JSON output, baseline comparison |
| Failure injection | 7/10 | 12 tests in tests/s3_failure_injection.rs (shard corruption, missing shards, streaming GET bitrot). no partition / slow-disk |
| Load / stress testing | 0/10 | none |
| CI | 6/10 | GitHub Actions: cargo test + cargo clippy. no benchmark CI |

## Operations and observability: 3/10

| Area | Rating | Notes |
|---|---|---|
| Admin API | 7/10 | status, disks, healing, inspect, bucket EC, cache flush endpoints |
| Structured logging | 3/10 | tracing spans + W3C server-timing per response. no metrics, no histograms, no error counters |
| Graceful shutdown | 7/10 | stops accepting, drains in-flight HTTP (5s), flushes write cache, drains WAL materialize worker. Ctrl+C only on Windows |
| Health checks | 4/10 | /_admin/status exists. no deep health probes |
| Monitoring integration | 0/10 | no prometheus, no opentelemetry |

## Packaging and deployment: 1/10

| Area | Rating | Notes |
|---|---|---|
| Binary releases | 0/10 | no releases |
| Dockerfile | 0/10 | does not exist |
| Package managers | 0/10 | not published anywhere |
| Upgrade path | 2/10 | volume.json carries `version` and is validated on load; no migration code, no segment/needle format version check yet |
| Configuration docs | 3/10 | CLI flags documented in code, no operator guide |
| CHANGELOG | 0/10 | does not exist |

## Documentation: 7/10

| Area | Rating | Notes |
|---|---|---|
| Architecture | 8/10 | thorough, audited, up to date |
| Write path | 8/10 | authoritative end-to-end trace with measured timings |
| Benchmarks | 8/10 | cross-server matrix, layered analysis, fairness spec |
| S3 compliance audit | 7/10 | per-operation table with ratings |
| Comparison | 8/10 | honest positioning against 8 competitors |
| Storage layout | 7/10 | on-disk format documented |
| Cluster design | 6/10 | goals and current scope clear, missing operational guidance |
| Operator / getting started guide | 2/10 | README quick start only |

## Performance: 8/10

| Area | Rating | Notes |
|---|---|---|
| Small object PUT (4KB) | 9/10 | L3: WAL 134us (7313 obj/s), file 693us (1344 obj/s). WAL 5.2x faster. L7 pending |
| Medium PUT (10MB) | 8/10 | L3: file 23.6ms (422 MB/s), WAL 25.9ms (380 MB/s). file wins 1.1x |
| Large PUT (1GB) | 8/10 | L3: file 2.26s (452 MB/s), WAL 3.20s (320 MB/s). file wins 1.4x |
| GET (mmap) | 8/10 | zero-copy 1+0, zero-alloc EC, 1220 MB/s at 1GB |
| Concurrent / parallel clients | 0/10 | not benchmarked, not tested |

---

## What blocks a v0.1.0 release

1. **Binary release** -- Dockerfile + GitHub release with Windows binary
2. **CHANGELOG** -- release notes
3. **Basic observability** -- request latency histograms, error counters, disk health

## What blocks production use

Everything above, plus:

1. **Encryption at rest** (SSE-S3 or equivalent)
2. **Partition / slow-disk failure injection** -- shard corruption and missing-shard paths are covered; network partitions and slow disks are not
3. **Live topology changes** -- cannot add/remove nodes without restart
4. **Concurrent client testing** -- all benchmarks are sequential single-client
5. **Monitoring integration** -- prometheus or opentelemetry
