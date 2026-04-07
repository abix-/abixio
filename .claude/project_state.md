# AbixIO Project State

## Last Session: 2026-04-06 (phase 2 s3s wiring in progress)

### What We Worked On
- Phase 2 s3s protocol layer: created all new files, wired main.rs
- s3_service.rs (1,024 lines, 31 S3 operations), s3_auth.rs, s3_access.rs, s3_route.rs
- main.rs wired to S3ServiceBuilder + AbixioDispatch
- Tests updated to use new dispatch layer
- 41/125 integration tests passing, 84 need behavioral fixes

### Decisions Made
- **dispatch layer instead of S3Route:** admin and storage_server use hyper Request<Incoming>
  which can't be constructed from s3s Body. AbixioDispatch intercepts _admin/* and
  _storage/v1/* at the hyper level before passing to s3s. cleaner than trying to
  convert between incompatible body types
- **no global state:** AbixioS3 struct owns Arc<VolumePool> + Arc<ClusterManager>
- **single map_err function:** all StorageError -> S3Error mapping in one place
- **body collection:** s3s returns streaming responses; AbixioDispatch collects
  the body stream into Full<Bytes> for hyper compatibility

### Current State
- Phase 2 IN PROGRESS: all new code compiles, 41/125 tests passing
- 84 failing tests are behavioral differences, not compilation errors
- Old src/s3/ code still exists (not yet deleted) -- some tests may still reference it
- plan at ~/.claude/plans/streamed-baking-dongarra.md

### Failing Test Categories (84 tests)
1. **multipart ops (~25):** multipart functions called directly, may need .await or signature fixes
2. **versioning (~8):** delete markers, version-id in responses
3. **CORS/notification/ACL stubs (~12):** s3s returns different status codes for unimplemented ops
4. **conditional requests (~6):** If-Match, If-None-Match not implemented in s3_service.rs
5. **status code/header differences (~15):** request-id, 405 vs 400, etc.
6. **XML format differences (~10):** smithy-generated XML vs our quick-xml templates
7. **policy/lifecycle/tagging (~8):** field mapping issues

### Next Steps
1. Fix 84 failing integration tests (biggest task -- categorize and batch-fix)
2. Delete old src/s3/ directory once all tests pass
3. Phase 3: cleanup deps, update docs

### k3s NodePort Allocation
| Port  | Service   |
|-------|-----------|
| 30080 | Ascender  |
