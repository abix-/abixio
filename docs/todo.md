# todo

## bugs

- [ ] object tagging (get/put/delete) returns service error
- [ ] inspect size off by 1 (17 vs 16)
- [ ] client relay returns chunk signatures in body (server not stripping chunked-transfer encoding)
- [ ] bucket delete fails when versioned objects exist

## critical

- [x] audit and eliminate unwrap() in non-test code (298 calls, 98 in volume_pool.rs alone -- panics kill a storage server)
- [ ] implement sigv4 chunked transfer auth (large file uploads via aws cli broken without it)
- [ ] add ci (github actions: cargo test + cargo clippy)

## high

- [ ] split s3/handlers.rs (1670 lines) into object/bucket/multipart handlers
- [ ] lifecycle endpoints store config but never enforce -- either implement background worker or return 501
- [ ] failure injection tests (kill volumes, corrupt shards, partition nodes mid-write)

## medium

- [ ] basic benchmark suite (put small, put large, get, list, delete with timing)
- [ ] release pipeline (dockerfile + github release with binaries)
- [ ] changelog -- no release notes exist for 138 commits

## low

- [ ] consensus-backed control plane (raft or equivalent) for cluster fencing edge cases under network partitions
