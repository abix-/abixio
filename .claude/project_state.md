# AbixIO Project State

## Last Session: 2026-04-05 (docs gap analysis + admin api docs)

### What We Worked On
Reviewed all 14 docs for coverage gaps against implemented features. Identified admin API (6 endpoints, 19 tests, zero docs) as the biggest gap. Created `docs/admin-api.md`, fixed stale project structure in architecture.md, corrected test counts in s3-compliance.md.

### Decisions Made
- **Admin API doc was the top priority gap** -- 6 fully implemented endpoints with no documentation at all.
- **Test count is 214** (101 unit + 94 S3 integration + 19 admin integration), not 215 as previously stated.
- **architecture.md project structure was missing** `router.rs`, `types.rs`, `admin/mod.rs`, `e2e.py`, `bucket-policy.md`.

### Current State
- **214 tests**, 0 failures
- **15 doc files** in docs/ (added admin-api.md)
- Commit 37afd07: admin api docs + architecture fixes + test count correction
- All implemented features now have dedicated documentation

### Architecture (key files)
- `src/storage/erasure_set.rs` -- `ErasureSet` with `resolve_ec()` per-request
- `src/storage/erasure_encode.rs` -- disk subset selection when pool > data+parity
- `src/storage/metadata.rs` -- `EcConfig` struct, `PutOptions.ec_data/ec_parity`
- `src/storage/disk.rs` -- `.ec.json` read/write on LocalDisk
- `src/admin/handlers.rs` -- bucket EC config endpoints

### Next Steps
- **Docs: CLI quickstart** -- no doc on how to run abixio (flags, env vars, auth modes, example commands)
- **Docs: standalone auth doc** -- SigV4 header auth + presigned + no-auth in one place (currently scattered)
- **Phase 2: Remote Backend** -- `RemoteDisk` implementing `Backend` over HTTP to another AbixIO instance. Coordinator pattern.
- **Phase 3: Pool Expansion** -- `ServerPools { pools: Vec<DiskPool> }`, proportional placement by free space.
- **Phase 4: Symmetric Multi-Node** -- every node runs full topology, any node accepts any request, inter-node RPC.
- Consider renaming `ErasureSet` to `DiskPool` (cosmetic, deferred from Phase 1 to minimize blast radius)

### k3s NodePort Allocation
| Port  | Service   |
|-------|-----------|
| 30080 | Ascender  |
