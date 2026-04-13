# todo

## critical

- [x] unwrap() audit. 533 unwrap() calls total, but 530 are in #[cfg(test)] blocks (standard for tests). 1 production unwrap in erasure_decode.rs:241 was guarded by a data_shards_ok check but replaced with let-else for clarity. 1 expect() in main.rs:31 is crypto provider init (standard). production code is clean
- [ ] log-structured storage: remaining phases. GC (phase 8), heal worker log-awareness (phase 7), versioned object support, chunked-transfer PUT support. S3 integration working for Content-Length PUTs <= 64KB. see docs/write-log.md
- [x] mc client throughput gap. root cause: StreamingBlob::wrap() used s3s StreamWrapper which returns RemainingLength::unknown(), causing hyper to use chunked transfer encoding instead of Content-Length. fix: SizedStream wrapper reports exact remaining_length. also fixed buffered GET path. result: mc GET 354->1476 MB/s (4.2x), now faster than curl
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

- [x] split tests/s3_integration.rs into 9 files by category: s3_bucket_ops, s3_object_ops, s3_multipart, s3_versioning, s3_tagging_conditional, s3_config_stubs, s3_ec, s3_hostile_input, s3_protocol. 125 tests, all passing
- [ ] fix test count lie. README says 355, docs/index.md says 171, todo.md said 329, actual #[test] count is 111 (expansion makes ~355). pick one source of truth, add a CI check that asserts it, delete wrong numbers
- [ ] refresh docs/index.md. "Current reality" block dated 2026-04-06 is stale: says 171 tests, lists conditional/versioning headers as missing (both done). fix or delete
- [ ] failure injection tests. kill a volume mid-write, corrupt a shard checksum, partition a remote node during multi-node PUT. erasure coding that has never been tested under actual faults is Reed-Solomon arithmetic, not fault tolerance
- [ ] document no-fsync ack semantics in README and write-path.md where users will see it. power loss eats recent writes. the README shows PUT/s numbers without mentioning the ack-from-page-cache tradeoff
- [ ] ship v0.1.0: Dockerfile + github release with windows binary. 347 commits, zero releases. no one can use this without building from source
- [ ] CHANGELOG. no release notes exist for 347 commits. add one, even if retroactive
- [ ] s3-compliance.md: POST policy uploads listed as Done 8/10 in auth section but PostPolicyBucket listed as No in operation table. accuracy report flags this as unresolved. pick one
- [ ] version-id response headers. x-amz-version-id and x-amz-delete-marker still "Pending" in s3-compliance.md
- [ ] bucket delete fails when versioned objects exist
- [ ] client relay returns chunk signatures in body (server not stripping chunked-transfer encoding)
- [x] wire versioning response headers (x-amz-version-id, x-amz-delete-marker) through s3s DTOs
- [x] implement conditional requests (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since) in s3_service.rs
- [x] lifecycle endpoints store config but never enforce. now stores and returns actual rules; enforcement still missing

## medium

- [ ] lifecycle enforcement. config is stored and returned but rules never execute. s3-compliance rates this 7/10, should be 3/10 until enforced. storing config you never enforce is worse than not implementing the feature because the user thinks it works
- [ ] bucket policy enforcement. policies stored but never checked on requests. same problem as lifecycle -- false sense of security
- [x] graceful shutdown. ctrl+c triggers: stop accepting connections, drain in-flight HTTP (5s timeout via hyper-util GracefulShutdown), flush write cache, drain pool rename workers (channel drain on shutdown signal). crash recovery for pool temps already existed. remaining: log segment seal on shutdown, multipart temp cleanup on startup
- [ ] observability. tracing is imported but no structured metrics, no request latency histograms, no error rate counters. no health endpoint beyond admin API. operators cannot tell if it is healthy or slowly dying
- [ ] encryption at rest (docs/encryption.md is design only, no implementation. listed in doc index like a feature)
- [x] comparative benchmark vs rustfs and minio: abixio-ui/src/bench/ with SVG charts in docs
- [x] per-layer benchmark suite (L1-L6) with JSON output and A/B comparison mode
- [x] basic benchmark suite: abixio-ui/src/bench/: PUT/GET/HEAD/LIST/DELETE with 1-4 disks
- [x] mmap GET fast path: 1+0 objects served via Bytes::from_owner(mmap), zero-copy. EC objects via mmap shard reads + 4MB decode blocks
- [x] versioning config cache: eliminates per-PUT disk read, L6 PUT +27%

## low

- [ ] consensus-backed control plane (raft or equivalent) for cluster fencing edge cases under network partitions
- [ ] inspect size off by 1 (17 vs 16)
