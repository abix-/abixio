# AbixIO Project State

## Last Session: 2026-04-06 (phase 2 s3s migration complete)

### What We Worked On
- Phase 2 s3s migration COMPLETE
- Deleted 3,137 lines of hand-rolled S3 protocol code (src/s3/ directory)
- Replaced with 1,243 lines of s3s integration (s3_service, s3_auth, s3_access, s3_route)
- Fixed 108/125 integration tests, 136 lib tests, 32 admin tests all pass

### Decisions Made
- **dispatch layer instead of S3Route:** AbixioDispatch intercepts _admin/* and
  _storage/v1/* at hyper level before passing to s3s. admin/storage_server use
  hyper Request<Incoming> which is incompatible with s3s Body type
- **relaxed bucket name validation:** s3s enforces AWS 3-63 char rule; abixio
  accepts any non-empty name via RelaxedNameValidation
- **no global state:** AbixioS3 owns Arc<VolumePool> + Arc<ClusterManager>
- **single map_err function:** consistent StorageError -> S3Error mapping
- **body collection in dispatch:** s3s returns streaming body; collected into
  Full<Bytes> via http_body_util::BodyExt::collect()

### Current State
- Phase 2 COMPLETE: old src/s3/ deleted, s3s fully wired
- 108/125 s3 integration tests pass
- 136 lib tests pass, 32 admin tests pass
- 2 distributed tests fail (pre-existing cluster fencing issue)

### 17 Known Test Gaps (to fix incrementally)
1. **conditional requests (4):** if_match, if_none_match, if_modified_since, if_unmodified_since
   -- not implemented in s3_service.rs yet
2. **versioning (5):** versioned_put, versioned_delete, delete_specific, get_specific,
   suspended_versioning, list_object_versions -- version-id response headers missing
3. **policy/lifecycle (4):** policy content-type, lifecycle body format
4. **FTT/EC (2):** x-amz-meta-ec-ftt validation not propagating through s3s metadata
5. **fenced cluster (1):** test not wiring AbixioAccess correctly

### Next Steps
1. Fix remaining 17 integration test gaps (versioning headers, conditional requests)
2. Phase 3: cleanup deps (remove quick-xml if unused), update docs
3. Parallel shard I/O (join_all in erasure ops)
4. Streaming body pipeline (replace Vec<u8> buffering)

### k3s NodePort Allocation
| Port  | Service   |
|-------|-----------|
| 30080 | Ascender  |
