# todo

## bugs

- [ ] object tagging (get/put/delete) returns service error
- [ ] inspect size off by 1 (17 vs 16)
- [ ] client relay returns chunk signatures in body (server not stripping chunked-transfer encoding)
- [ ] bucket delete fails when versioned objects exist

## critical

- [x] audit and eliminate unwrap() in non-test code (298 calls, 98 in volume_pool.rs alone -- panics kill a storage server)
- [x] implement sigv4 chunked transfer auth (handled by s3s protocol layer)
- [x] migrate to s3s protocol layer (replaced 3,137 lines hand-rolled code with 1,243 lines s3s integration)
- [x] make storage layer async (Backend + Store traits, LocalVolume, RemoteVolume, VolumePool)
- [ ] add ci (github actions: cargo test + cargo clippy)

## high

- [ ] wire versioning response headers (x-amz-version-id, x-amz-delete-marker) through s3s DTOs
- [ ] implement conditional requests (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since) in s3_service.rs
- [ ] lifecycle endpoints store config but never enforce -- either implement background worker or return 501
- [ ] failure injection tests (kill volumes, corrupt shards, partition nodes mid-write)

## medium

- [ ] basic benchmark suite (put small, put large, get, list, delete with timing)
- [ ] release pipeline (dockerfile + github release with binaries)
- [ ] changelog -- no release notes exist for 138 commits

## low

- [ ] consensus-backed control plane (raft or equivalent) for cluster fencing edge cases under network partitions
