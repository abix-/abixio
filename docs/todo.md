# todo

## critical

- [ ] mc client throughput gap -- AbixIO serves 1GB at 1220 MB/s via curl but only 354 MB/s through mc. MinIO gets 970 MB/s through the same mc. profile with mc --debug, diff headers, find the bottleneck. this is not a client problem
- [ ] unwrap() plague -- 252 calls in src/ (volume_pool.rs: 108, local_volume.rs: 53, cluster/mod.rs: 27, heal/worker.rs: 21). every one is a process crash. commit e9a38aa claimed to fix this, they came back. go through each one: impossible? .expect("reason"). possible? propagate with ?
- [x] EC GET regression -- fixed. zero-alloc fast path slices directly from mmap when all shards healthy. 4-disk GET: 803->1236 MB/s (+54%), now +35% above pre-mmap baseline (919)
- [ ] RemoteVolume::bucket_exists() returns false unconditionally -- cluster bucket ops partially broken. this is a shipped bug, not a TODO
- [x] streaming body support -- unified encode path via ShardWriter trait, inline MD5+blake3, no full-body buffering
- [x] s3s GET response buffering -- streaming GET via read_and_decode_stream + get_object_stream + mmap fast path. L6 GET: 365->1048 MB/s (1GB). curl: 1220 MB/s
- [x] parallel shard writes in streaming path -- tested FuturesUnordered and channel-based tasks, both regressed. sequential is faster on same physical disk
- [x] implement sigv4 chunked transfer auth (handled by s3s protocol layer)
- [x] migrate to s3s protocol layer (replaced 3,137 lines hand-rolled code with 1,243 lines s3s integration)
- [x] make storage layer async (Backend + Store traits, LocalVolume, RemoteVolume, VolumePool)
- [x] add ci (github actions: cargo test + cargo clippy)
- [x] fix 17 regressed s3 integration tests from s3s migration

## high

- [ ] ship v0.1.0 -- Dockerfile + github release with windows binary. 210 commits, zero releases. no one can use this without building from source
- [ ] split tests/s3_integration.rs (3,935 lines) by operation category -- bucket ops, object ops, multipart, versioning, tagging, conditional, hostile input
- [ ] CHANGELOG -- no release notes exist for 210 commits. add one, even if retroactive
- [ ] failure injection tests (kill volumes, corrupt shards, partition nodes mid-write)
- [ ] version-id response headers -- x-amz-version-id and x-amz-delete-marker still "Pending" in s3-compliance.md
- [ ] bucket delete fails when versioned objects exist
- [ ] client relay returns chunk signatures in body (server not stripping chunked-transfer encoding)
- [ ] README test count wrong -- says 171, actual is 298. update it
- [x] wire versioning response headers (x-amz-version-id, x-amz-delete-marker) through s3s DTOs
- [x] implement conditional requests (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since) in s3_service.rs
- [x] lifecycle endpoints store config but never enforce -- now stores and returns actual rules; enforcement still missing

## medium

- [ ] lifecycle enforcement -- config is stored and returned but rules never execute. s3-compliance rates this 7/10, should be 3/10 until enforced
- [ ] bucket policy enforcement -- policies stored but never checked on requests. same problem as lifecycle
- [ ] graceful shutdown -- what happens to in-flight writes? do shard writers finalize? do partial .abixio.tmp dirs get cleaned up on startup?
- [ ] observability -- tracing is imported but no structured metrics, no request latency histograms, no error rate counters. operators need to know if it's healthy
- [ ] encryption at rest (docs/encryption.md is design only, no implementation)
- [x] comparative benchmark vs rustfs and minio -- tests/compare_bench.sh with SVG charts in docs
- [x] per-layer benchmark suite (L1-L6) with JSON output and A/B comparison mode
- [x] basic benchmark suite -- abixio-ui/tests/bench.rs: PUT/GET/HEAD/LIST/DELETE with 1-4 disks
- [x] mmap GET fast path -- 1+0 objects served via Bytes::from_owner(mmap), zero-copy. EC objects via mmap shard reads + 4MB decode blocks
- [x] versioning config cache -- eliminates per-PUT disk read, L6 PUT +27%

## low

- [ ] consensus-backed control plane (raft or equivalent) for cluster fencing edge cases under network partitions
- [ ] inspect size off by 1 (17 vs 16)
