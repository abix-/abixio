# AbixIO Project State

## Last Session: 2026-04-05 (per-object EC)

### What We Worked On
Researched how MinIO handles distributed multi-node architecture vs AbixIO's single-process model. Designed and implemented per-object erasure coding as AbixIO's key differentiator -- every object owns its own data/parity scheme, stored in metadata. This is the foundation for future multi-node work.

### Decisions Made
- **Per-object EC over cluster-level EC:** MinIO locks EC ratio per server pool. AbixIO makes it per-object, which is strictly more flexible. Each object's `meta.json` already stored erasure params -- we just made the encode path honor them.
- **Precedence chain:** per-object header (`x-amz-meta-ec-data/parity`) > bucket config (`.ec.json` via admin API) > server default (`--data`/`--parity`)
- **S3-compatible approach:** `x-amz-meta-*` headers are standard S3 custom metadata. Bucket EC config lives in admin API (non-S3 surface), no compat issues.
- **Disk pool model:** `--disks` is now a pool. `disk_count >= data+parity` (was `==`). Objects select a subset of disks via `hash_order()`.
- **No GCD algorithm:** Unlike MinIO's opaque GCD-based erasure set sizing, AbixIO keeps it simple -- user controls EC via data/parity, disk count is the pool size.
- **Heal derives EC from meta:** Heal workers read data/parity from each object's stored metadata, not from global config. This is essential for mixed-EC pools.
- **Full roadmap approved (phases 1-4):** Phase 1 (per-object EC) done. Phase 2 (RemoteDisk backend), Phase 3 (pool expansion), Phase 4 (symmetric multi-node) planned.

### Current State
- **Per-object EC fully implemented and tested** (commit bd1f604)
- **203 tests** (102 unit + 13 admin integration + 88 S3 integration), 0 failures
- 0 new clippy warnings (26 pre-existing)
- Admin API: `GET/PUT /_admin/bucket/{name}/ec` for bucket EC config
- S3 PUT: `x-amz-meta-ec-data` / `x-amz-meta-ec-parity` headers for per-object EC
- Docs updated: README, architecture.md, storage-layout.md

### Architecture (key files)
- `src/storage/erasure_set.rs` -- `ErasureSet` with `resolve_ec()` per-request
- `src/storage/erasure_encode.rs` -- disk subset selection when pool > data+parity
- `src/storage/metadata.rs` -- `EcConfig` struct, `PutOptions.ec_data/ec_parity`
- `src/storage/disk.rs` -- `.ec.json` read/write on LocalDisk
- `src/admin/handlers.rs` -- bucket EC config endpoints

### Next Steps
- **Phase 2: Remote Backend** -- `RemoteDisk` implementing `Backend` over HTTP to another AbixIO instance. Coordinator pattern.
- **Phase 3: Pool Expansion** -- `ServerPools { pools: Vec<DiskPool> }`, proportional placement by free space.
- **Phase 4: Symmetric Multi-Node** -- every node runs full topology, any node accepts any request, inter-node RPC.
- Consider renaming `ErasureSet` to `DiskPool` (cosmetic, deferred from Phase 1 to minimize blast radius)

### k3s NodePort Allocation
| Port  | Service   |
|-------|-----------|
| 30080 | Ascender  |
