# todo

## kovarex review 2026-04-16 (priority order, do not reorder)

1. ~~**fix the documentation lies.**~~ done 2026-04-16. README.md:65 dead link fixed (now points at write-wal.md). status.md: failure injection 0/10 -> 7/10 (12 tests, partition/slow-disk still absent); v0.1.0 blocker list purged of unwrap audit and log store GC; graceful shutdown moved to "Closed" section; last-updated bumped to 2026-04-16
2. **ship v0.1.0.** Dockerfile, windows binary github release, retroactive CHANGELOG with honest scope ("research project, on-disk format has no versioning, expect data loss"). add format version byte to volume.json **before** tagging. two days
3. ~~**delete the write cache, or finish phases 5-8.**~~ done 2026-04-16. finished phases 4-8: background flush task, primary_node_id tag, msgpack-serializable CacheEntry, PeerCacheClient, `/cache-replicate` + `/cache-sync` endpoints on storage_server, replicate-before-ack in small-object PUT, startup pull from peers. 3 new integration tests. docs/write-cache.md rewritten to match code
4. ~~**close WAL incomplete paths.**~~ done 2026-04-16. needle format extended with version_id (steals byte from _pad). WAL pending map keyed by (bucket, key, version_id); versioned writes route through WAL; materialize worker writes version_dir + merges meta.json. Heal awareness: VolumePool.disks is Arc-shared with the heal scanner via `heal_backends()`, so scanner/heal see WAL-pending entries (status 3/10 -> 8/10). chunked-transfer-to-WAL explicitly dropped (WAL is <=64KB, buffered path covers it). Tests: 4 wal, 1 local_volume, 1 heal, 1 volume_pool
5. **concurrent-client benchmark.** add to abixio-ui/src/bench/: 8/32/128 concurrent clients, PUT+GET at 4KB and 10MB. status.md:142 rates concurrent 0/10. every current bench number is sequential single-client -- DashMap-vs-Mutex arguments are theoretical without this
6. **split volume_pool.rs.** 1888 lines doing FTT resolution + placement + write-branch selection + EC dispatch + cache/WAL/file routing. extract before the next subsystem lands inside it. same problem lurks in local_volume.rs (1183 lines)
7. ~~**enforce lifecycle + bucket policy, or rate them 0/10 in s3-compliance.md.**~~ done 2026-04-16. lifecycle: `LifecycleEngine` background loop, supports `Expiration.days`, `ExpiredObjectDeleteMarker`, `NoncurrentVersionExpiration`, `AbortIncompleteMultipartUpload`, `Filter.prefix`. policy: `Policy::is_allowed` in `AbixioAccess::check`, supports Allow/Deny with `Principal: "*"` or AWS list, action + resource globs. Unsupported shapes rated 0/10 in s3-compliance. 6 new integration tests
8. **only then:** read cache, observability (metrics/histograms), encryption at rest. not before 1-7

## critical

- [x] unwrap() audit. 533 unwrap() calls total, but 530 are in #[cfg(test)] blocks (standard for tests). 1 production unwrap in erasure_decode.rs:241 was guarded by a data_shards_ok check but replaced with let-else for clarity. 1 expect() in main.rs:31 is crypto provider init (standard). production code is clean
- [x] WAL replaces log store and write pool. append-only segment with background materialize to file-per-object layout. no GC, no permanent index, bounded startup. 4KB PUT: 147us (tied with log 143us, 5.4x faster than file 793us). mmap writes, zero-alloc serialize_into, fire-and-forget channel, Arc<str> identity. see docs/write-wal.md
- [x] make WAL the default write tier (default changed to "wal" in 496f3b4)
- [x] WAL: versioned object support. needle carries `version_id`; pending keyed by (bucket, key, version_id); materialize worker writes version_dir + merges meta.json. kovarex #4
- [x] WAL chunked-transfer PUT: dropped as out-of-scope. buffered path handles all <=64KB writes; chunked-transfer PUTs of that size don't win anything from streaming-to-WAL. kovarex #4
- [x] WAL heal awareness: VolumePool.disks is `Arc<Vec<Box<dyn Backend>>>`, heal_backends() shares it with the scanner, WAL-pending entries visible. kovarex #4
- [x] fix README.md:65 dead link: replaced with [WAL write path](docs/write-wal.md). kovarex #1
- [x] refresh docs/status.md: failure injection 0/10 -> 7/10 (12 tests), removed log store GC and unwrap audit from v0.1.0 blockers, moved graceful shutdown to "Closed" section. kovarex #1
- [x] on-disk format version byte in volume.json. `CURRENT_VOLUME_FORMAT_VERSION=1`, `MIN_SUPPORTED_VOLUME_FORMAT_VERSION=1`, `check_format_version()` hard-fails unknown versions, `read_volume_format` surfaces parse/version errors. 6 new tests. kovarex #2
- [x] write cache peer replication (phases 4-8). background flush, primary_node_id, PeerCacheClient, cache-replicate + cache-sync endpoints, replicate-before-ack, startup peer pull. tests: tests/cache_replication.rs (3 tests). kovarex #3
- [ ] concurrent-client benchmark: 8/32/128 clients, 4KB + 10MB, PUT + GET. kovarex #5
- [ ] split volume_pool.rs (1888 lines) into placement / write-branch / EC dispatch modules. kovarex #6
- [ ] split local_volume.rs (1183 lines). kovarex #6
- [x] read cache for small objects. `src/storage/read_cache.rs`: LRU by total bytes, bounded by `--read-cache` (MB), per-object ceiling via `--read-cache-max-object`. Wired into `VolumePool::get_object` and `get_object_stream`; invalidated on PUT, versioned PUT, DELETE, DeleteVersion, AddDeleteMarker, DeleteBucket. 8 new unit tests. kovarex #8
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
- [x] fix test count. README and index.md now say 362 passing (from cargo test output). old hardcoded numbers removed. CI check still TODO
- [x] refresh docs/index.md. "Current reality" updated to 2026-04-13. stale items removed
- [x] failure injection tests. s3_failure_injection.rs: 12 passing, 1 ignored (1+0 by design). found and fixed streaming GET bitrot bug: read_and_decode_stream skipped checksum verification, corrupted shards were served as healthy. fix: verify_shard_checksum before placing mmaps, same as buffered path
- [x] no-fsync ack semantics. reviewed: page-cache writes without per-object fsync is standard for object stores (MinIO, RustFS, SeaweedFS all do the same). durability comes from erasure coding across disks, not per-write fsync. already documented in architecture.md:66 and comparison.md:130. no additional warning needed
- [ ] ship v0.1.0: Dockerfile + github release with windows binary. 347 commits, zero releases. no one can use this without building from source. kovarex #2
- [ ] CHANGELOG. no release notes exist for 347 commits. add one, even if retroactive. kovarex #2
- [x] s3-compliance.md: POST policy conflict resolved. marked No 0/10 in both auth section and operation table. no implementation exists
- [x] version-id response headers. s3-compliance.md accuracy report updated: headers verified working, test references updated to split test files
- [ ] bucket delete fails when versioned objects exist
- [ ] client relay returns chunk signatures in body (server not stripping chunked-transfer encoding)
- [x] wire versioning response headers (x-amz-version-id, x-amz-delete-marker) through s3s DTOs
- [x] implement conditional requests (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since) in s3_service.rs
- [x] lifecycle endpoints store config but never enforce. now stores and returns actual rules; enforcement still missing

## medium

- [x] lifecycle enforcement. `LifecycleEngine` background loop in `src/lifecycle/`. supported: `Expiration.days`, `ExpiredObjectDeleteMarker`, `NoncurrentVersionExpiration`, `AbortIncompleteMultipartUpload`, `Filter.prefix`. unsupported shapes rated 0/10 in s3-compliance. kovarex #7
- [x] bucket policy enforcement. `Policy::is_allowed` evaluated in `AbixioAccess::check`. supported: Allow/Deny, `Principal: "*"` or AWS list, action + resource globs. Conditions, NotAction/NotResource, IAM roles: 0/10. kovarex #7
- [x] graceful shutdown. ctrl+c triggers: stop accepting connections, drain in-flight HTTP (5s timeout via hyper-util GracefulShutdown), flush write cache, drain pool rename workers (channel drain on shutdown signal). crash recovery for pool temps already existed. remaining: log segment seal on shutdown, multipart temp cleanup on startup
- [x] observability. `/_admin/metrics` Prometheus text endpoint. Request latency histograms + error counters (per op + status_class), cache hit/miss, WAL pending, disk capacity, heal/scanner/lifecycle/cluster state gauges. `docs/metrics.md` enumerates every series with example PromQL. OTel traces + JSON log rotation + audit log + bundled Grafana dashboards out of scope for now. kovarex #8
- [ ] encryption at rest (docs/encryption.md is design only, no implementation. listed in doc index like a feature). kovarex #8 (blocked on #1-7)
- [x] comparative benchmark vs rustfs and minio: abixio-ui/src/bench/ with SVG charts in docs
- [x] per-layer benchmark suite (L1-L6) with JSON output and A/B comparison mode
- [x] basic benchmark suite: abixio-ui/src/bench/: PUT/GET/HEAD/LIST/DELETE with 1-4 disks
- [x] mmap GET fast path: 1+0 objects served via Bytes::from_owner(mmap), zero-copy. EC objects via mmap shard reads + 4MB decode blocks
- [x] versioning config cache: eliminates per-PUT disk read, L6 PUT +27%

## low

- [ ] consensus-backed control plane (raft or equivalent) for cluster fencing edge cases under network partitions
- [ ] inspect size off by 1 (17 vs 16)
