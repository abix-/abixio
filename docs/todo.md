# todo

## critical

- [ ] log-structured storage: remaining phases. GC (phase 8), heal worker log-awareness (phase 7), versioned object support, chunked-transfer PUT support. S3 integration working for Content-Length PUTs <= 64KB. see docs/write-log.md
- [ ] mc client throughput gap. AbixIO serves 1GB at 1220 MB/s via curl but only 354 MB/s through mc. MinIO gets 970 MB/s through the same mc. profile with mc --debug, diff headers, find the bottleneck
- [ ] unwrap() plague. 374 calls in src/ (was 252, growing). hot files: local_volume.rs 75, cluster/mod.rs 27, heal/worker.rs 21 + 1 panic!. failure-path code is the one path that must not panic
- [x] debug header in src/s3_route.rs (x-debug-s3s-ms). kept and extended: now also emits W3C `server-timing` header with per-layer breakdown (setup, validate, ec_encode, storage_write, etc). `src/timing.rs` module with tokio task_local, RAII Span, 7 unit tests. the "remove in production" comment was wrong -- it's a profiling feature, not debug clutter
- [x] EC GET regression fixed. zero-alloc fast path slices directly from mmap. 4-disk GET: 803->1236 MB/s
- [x] log-structured storage: needle.rs + segment.rs + log_store.rs + S3 integration. small PUTs with Content-Length <= 64KB route through log store end-to-end. verified with curl. 4 appends vs 12 fs ops per 4KB object on 4 disks
- [x] RemoteVolume::bucket_exists() and bucket_created_at() now hit the remote peer endpoints instead of returning hardcoded false/0. Backend trait methods made async; all callers (volume_pool make_bucket/head_bucket/list_buckets, storage_server handlers, test mocks) updated. 12 bucket-related lib tests pass.
- [x] streaming body support: unified encode path via ShardWriter trait, inline MD5+blake3, no full-body buffering
- [x] s3s GET response buffering: streaming GET via read_and_decode_stream + get_object_stream + mmap fast path. L6 GET: 365->1048 MB/s (1GB). curl: 1220 MB/s
- [x] parallel shard writes in streaming path: tested FuturesUnordered and channel-based tasks, both regressed. sequential is faster on same physical disk
- [x] implement sigv4 chunked transfer auth (handled by s3s protocol layer)
- [x] migrate to s3s protocol layer (replaced 3,137 lines hand-rolled code with 1,243 lines s3s integration)
- [x] make storage layer async (Backend + Store traits, LocalVolume, RemoteVolume, VolumePool)
- [x] add ci (github actions: cargo test + cargo clippy)
- [x] fix 17 regressed s3 integration tests from s3s migration

## high

- [ ] ship v0.1.0: Dockerfile + github release with windows binary. 210 commits, zero releases. no one can use this without building from source
- [ ] split tests/s3_integration.rs (3,935 lines) by operation category: bucket ops, object ops, multipart, versioning, tagging, conditional, hostile input
- [ ] CHANGELOG. no release notes exist for 210 commits. add one, even if retroactive
- [ ] failure injection tests (kill volumes, corrupt shards, partition nodes mid-write)
- [ ] version-id response headers. x-amz-version-id and x-amz-delete-marker still "Pending" in s3-compliance.md
- [ ] bucket delete fails when versioned objects exist
- [ ] client relay returns chunk signatures in body (server not stripping chunked-transfer encoding)
- [ ] doc test counts disagree. README says 320, docs/index.md says 171, actual #[test] count is 329. fix README and docs/index.md, then add a CI check that asserts both match reality so drift fails the build
- [ ] docs/index.md "Current reality" block dated 2026-04-06 is stale. says 171 tests, lists conditional/versioning headers as missing (both done). refresh it
- [x] wire versioning response headers (x-amz-version-id, x-amz-delete-marker) through s3s DTOs
- [x] implement conditional requests (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since) in s3_service.rs
- [x] lifecycle endpoints store config but never enforce. now stores and returns actual rules; enforcement still missing

## medium

- [ ] lifecycle enforcement. config is stored and returned but rules never execute. s3-compliance rates this 7/10, should be 3/10 until enforced
- [ ] bucket policy enforcement. policies stored but never checked on requests. same problem as lifecycle
- [ ] graceful shutdown. what happens to in-flight writes? do shard writers finalize? do partial .abixio.tmp dirs get cleaned up on startup?
- [ ] observability. tracing is imported but no structured metrics, no request latency histograms, no error rate counters. operators need to know if it's healthy
- [ ] encryption at rest (docs/encryption.md is design only, no implementation)
- [x] comparative benchmark vs rustfs and minio: tests/compare_bench.sh with SVG charts in docs
- [x] per-layer benchmark suite (L1-L6) with JSON output and A/B comparison mode
- [x] basic benchmark suite: abixio-ui/tests/bench.rs: PUT/GET/HEAD/LIST/DELETE with 1-4 disks
- [x] mmap GET fast path: 1+0 objects served via Bytes::from_owner(mmap), zero-copy. EC objects via mmap shard reads + 4MB decode blocks
- [x] versioning config cache: eliminates per-PUT disk read, L6 PUT +27%

## low

- [ ] consensus-backed control plane (raft or equivalent) for cluster fencing edge cases under network partitions
- [ ] inspect size off by 1 (17 vs 16)
