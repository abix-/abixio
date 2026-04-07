# todo

## critical

- [x] streaming body support -- unified encode path via ShardWriter trait, inline MD5+blake3, no full-body buffering
- [x] s3s GET response buffering -- streaming GET via read_and_decode_stream + get_object_stream. L6 GET: 365->750 MB/s (10MB), 452->833 MB/s (1GB). range/versioned requests still use buffered path
- [x] parallel shard writes in streaming path -- tested FuturesUnordered and channel-based tasks, both regressed (-21% at 4-disk 10MB). sequential is faster when all shards are on the same physical disk. only helps with truly separate physical disks
- [ ] unwrap() regression -- 249 calls in src/ (volume_pool.rs: 104, cluster/mod.rs: 27, heal/worker.rs: 21). commit e9a38aa claimed to eliminate all production unwrap() but they came back. every one is a potential process crash
- [x] implement sigv4 chunked transfer auth (handled by s3s protocol layer)
- [x] migrate to s3s protocol layer (replaced 3,137 lines hand-rolled code with 1,243 lines s3s integration)
- [x] make storage layer async (Backend + Store traits, LocalVolume, RemoteVolume, VolumePool)
- [x] add ci (github actions: cargo test + cargo clippy)
- [x] fix 17 regressed s3 integration tests from s3s migration

## high

- [ ] version-id response headers -- x-amz-version-id and x-amz-delete-marker still "Pending" in s3-compliance.md. versioning without these headers breaks any S3 client that checks them
- [ ] failure injection tests (kill volumes, corrupt shards, partition nodes mid-write)
- [ ] split tests/s3_integration.rs (3,935 lines) by operation category
- [ ] RemoteVolume::bucket_exists() returns false unconditionally -- cluster bucket ops partially broken
- [ ] bucket delete fails when versioned objects exist
- [ ] client relay returns chunk signatures in body (server not stripping chunked-transfer encoding)
- [ ] inspect size off by 1 (17 vs 16)
- [x] wire versioning response headers (x-amz-version-id, x-amz-delete-marker) through s3s DTOs
- [x] implement conditional requests (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since) in s3_service.rs
- [x] lifecycle endpoints store config but never enforce -- now stores and returns actual rules; enforcement still missing

## medium

- [ ] lifecycle enforcement -- config is stored and returned but rules never execute. s3-compliance rates this 7/10, should be 3/10 until enforced
- [ ] bucket policy enforcement -- policies stored but never checked on requests. same problem as lifecycle
- [ ] release pipeline (dockerfile + github release with binaries). 180 commits, zero releases
- [ ] changelog -- no release notes exist for 180 commits
- [ ] encryption at rest (docs/encryption.md is design only, no implementation)
- [x] comparative benchmark vs rustfs and minio -- tests/compare_bench.sh with SVG charts in docs
- [x] per-layer benchmark suite (L1-L6) with JSON output and A/B comparison mode
- [x] basic benchmark suite -- abixio-ui/tests/bench.rs: PUT/GET/HEAD/LIST/DELETE with 1-4 disks

## low

- [ ] consensus-backed control plane (raft or equivalent) for cluster fencing edge cases under network partitions
