# AbixIO Project State

## Last Session: 2026-04-04

### What We Worked On
Wired background healing -- the last major missing piece before MVP. Reviewed MinIO's healing implementation in depth (8 files: mrf.go, erasure-healing.go, erasure-healing-common.go, erasure-decode.go, background-heal-ops.go, global-heal.go, background-newdisks-heal-ops.go, erasure-object.go) and adapted their three-path architecture for AbixIO's single-process model.

### Decisions Made
- Adapted MinIO's MRF + scanner two-path healing (skipped their third path: background heal workers as a separate dispatch layer -- unnecessary for single process)
- heal_object() uses metadata consensus by quorum_eq majority (matches MinIO's modtime consensus approach)
- Single MRF drain worker (single tokio::mpsc receiver), not multiple workers -- sufficient at our scale
- No MRF persistence to disk on shutdown (MinIO does this) -- scanner catches anything lost
- No namespace locking during heal -- single process, concurrent write just triggers re-heal
- Graceful shutdown via tokio::sync::watch channel (avoided adding tokio-util dependency for CancellationToken)
- encode_and_write_with_mrf() with #[allow(clippy::too_many_arguments)] rather than refactoring into a struct -- kept minimal diff

### Current State
- **Progress: ~95%** (up from ~85%)
- **101 tests** (88 unit + 13 integration), 0 clippy warnings
- Healing fully wired: MRF auto-enqueue on partial writes, scanner loop, heal_object() with Reed-Solomon reconstruction, graceful shutdown
- All existing functionality preserved (S3 API, erasure coding, bitrot detection, auth)
- MinIO source cloned to C:\code\minio for reference

### Files Changed This Session
- `src/heal/worker.rs` -- NEW (~290 lines): heal_object(), mrf_drain_worker(), scanner_loop(), 8 tests
- `src/heal/mod.rs` -- added worker module
- `src/storage/erasure_encode.rs` -- MRF auto-enqueue on partial write success
- `src/storage/erasure_set.rs` -- holds Arc<MrfQueue>, passes to encode
- `src/main.rs` -- wiring: MRF queue, scan state, spawn workers, graceful shutdown
- `PLAN.md` -- detailed healing architecture docs
- `README.md` -- updated status

### Next Steps
- Multipart upload (needed for files >8MB with AWS CLI)
- Object versioning
- Bucket replication
- Consider MRF persistence to disk on shutdown (nice-to-have)
- Live smoke test of healing: run server, PUT, rm shard, watch scanner heal, GET
