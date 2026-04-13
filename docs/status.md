# Project status

Completion ratings for every area that matters before AbixIO can be
called production-ready. Rated 1-10 where 1 = concept only, 5 = works
but gaps remain, 10 = production-grade.

Last updated: 2026-04-13

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
| Write path (file tier) | 8/10 | direct mkdir+write, simple, correct |
| Write path (log store) | 6/10 | working for <=64KB non-versioned PUTs, no GC yet, no versioned object support, no chunked-transfer PUT |
| Write path (pool) | 7/10 | pre-opened slots, async rename, pending-reads work. no versioned support |
| Write cache (RAM) | 5/10 | DashMap insert works, but no automatic flush, no eviction policy, not exercised by benchmarks |
| Read path (GET) | 8/10 | mmap zero-copy for 1+0, zero-alloc mmap slices for EC, log index lookup, range support |
| Streaming large objects | 7/10 | unified ShardWriter encode path, no full-body buffering. streaming GET works |
| Volume pool / placement | 7/10 | deterministic placement, quorum writes, multi-disk spread. no rebalance |
| Metadata / on-disk format | 7/10 | self-describing volumes, meta.json per object, volume.json identity |

## S3 API coverage: 5/10

41 of 72 operations implemented (57%).

| Area | Rating | Notes |
|---|---|---|
| Object CRUD (PUT/GET/HEAD/DELETE) | 9/10 | conditionals, versioning, per-object EC, range, metadata |
| Object listing | 7/10 | v2, versions, prefix/delimiter. no v1 |
| Multipart upload | 8/10 | create, upload part, complete, abort, list parts. no CopyObjectPart |
| Bucket operations | 7/10 | create, delete, head, list, versioning, tagging, policy, lifecycle config |
| Bucket policy enforcement | 2/10 | config stored, never checked on requests |
| Lifecycle enforcement | 2/10 | config stored, rules never execute |
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
| Path traversal hardening | 8/10 | fixed in commit 9e25e79, regression tests in place |
| Internode JWT auth | 7/10 | signed with S3 credentials, validates on receive |
| Encryption at rest | 0/10 | design doc only, no implementation |
| TLS | 7/10 | works with rustls, cert/key from CLI flags. no auto-cert |

## Clustering: 5/10

| Area | Rating | Notes |
|---|---|---|
| Node identity exchange | 7/10 | automatic at startup via --nodes |
| Self-describing volumes | 8/10 | volume.json on every volume, cluster reconstructable from disks |
| Internode shard RPC | 7/10 | RemoteVolume over HTTP, JWT auth |
| Quorum tracking | 6/10 | basic probe-based, fences on quorum loss |
| Hard fencing | 7/10 | rejects S3 and mutating admin requests when unsafe |
| Placement metadata | 7/10 | epoch_id + volume_ids in every shard |
| Live topology changes | 0/10 | not implemented. nodes fixed at startup |
| Rebalance | 0/10 | not implemented |
| Consensus (Raft etc.) | 0/10 | not implemented |

## Healing and integrity: 6/10

| Area | Rating | Notes |
|---|---|---|
| MRF (reactive heal) | 7/10 | bounded dedup queue, drain worker, triggers on partial write failure |
| Integrity scanner | 6/10 | walks all objects, verifies checksums, per-object EC aware. single worker |
| Per-shard checksums | 8/10 | blake3, SIMD-accelerated, checked on read |
| Log store heal awareness | 3/10 | not implemented (todo phase 7) |
| Failure injection tests | 0/10 | none exist |

## Testing: 5/10

| Area | Rating | Notes |
|---|---|---|
| Unit tests (lib) | 6/10 | 111 tests in src/. covers core paths |
| Integration tests | 6/10 | 162 tests in tests/. S3 ops, admin, distributed placement |
| Benchmark suite | 8/10 | L1-L7 layered benchmarks, competitive matrix, JSON output, baseline comparison |
| Failure injection | 0/10 | none |
| Load / stress testing | 0/10 | none |
| CI | 6/10 | GitHub Actions: cargo test + cargo clippy. no benchmark CI |

## Operations and observability: 3/10

| Area | Rating | Notes |
|---|---|---|
| Admin API | 7/10 | status, disks, healing, inspect, bucket EC endpoints |
| Structured logging | 3/10 | tracing imported, no metrics, no histograms, no error counters |
| Graceful shutdown | 1/10 | not implemented. in-flight writes may corrupt |
| Health checks | 4/10 | /_admin/status exists. no deep health probes |
| Monitoring integration | 0/10 | no prometheus, no opentelemetry |

## Packaging and deployment: 1/10

| Area | Rating | Notes |
|---|---|---|
| Binary releases | 0/10 | 347 commits, zero releases |
| Dockerfile | 0/10 | does not exist |
| Package managers | 0/10 | not published anywhere |
| Upgrade path | 0/10 | on-disk format has no versioning or migration |
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
| Small object PUT (4KB) | 9/10 | 1716 obj/s via SDK+TLS, 4.7x MinIO, 5.7x RustFS |
| Medium PUT (10MB) | 8/10 | 421 MB/s (pool tier), +35% over competitors |
| Large PUT (1GB) | 8/10 | 535 MB/s (log tier), +35% over MinIO |
| GET (mmap) | 8/10 | zero-copy 1+0, zero-alloc EC, 1220 MB/s at 1GB |
| Concurrent / parallel clients | 0/10 | not benchmarked, not tested |

---

## What blocks a v0.1.0 release

These are the minimum items before anyone should use this:

1. **Graceful shutdown** -- in-flight writes must not corrupt data
2. **Binary release** -- Dockerfile + GitHub release with Windows binary
3. **CHANGELOG** -- retroactive release notes
4. **On-disk format version** -- so future versions can detect and migrate
5. **unwrap() audit** -- 533 unwrap() calls in src/. failure paths must not panic
6. **Log store GC** -- without garbage collection, log segments grow without bound
7. **Basic observability** -- request latency, error rates, disk health

## What blocks production use

Everything above, plus:

1. **Encryption at rest** (SSE-S3 or equivalent)
2. **Lifecycle enforcement** -- rules stored but never enforced
3. **Bucket policy enforcement** -- policies stored but never checked
4. **Failure injection testing** -- kill volumes, corrupt shards, partition nodes
5. **Live topology changes** -- cannot add/remove nodes without restart
6. **Concurrent client testing** -- all benchmarks are sequential single-client
7. **Monitoring integration** -- prometheus or opentelemetry
