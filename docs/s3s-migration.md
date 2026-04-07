# s3s migration plan

migrate the protocol layer to s3s and make the storage layer async.

## why

abixio has 41 S3 operations across ~3,100 lines of hand-rolled protocol code
(auth, routing, XML, error formatting). every new protocol feature (chunked
auth, trailing checksums, POST policy) is more code to write and maintain.
s3s is the only maintained, permissively-licensed rust crate for S3 server-side
protocol handling (218k downloads/mo, 29 contributors, apache 2.0, v0.13
march 2026). at 2 days old, abixio is at the cheapest switching point.

the storage layer is synchronous (std::fs, reqwest::blocking). a single PUT
to a 6-disk cluster with 3 remote volumes blocks the tokio thread for 3
sequential HTTP round-trips. making Backend and Store async fixes this and
aligns with s3s's async S3 trait.

## what stays

everything that makes abixio abixio:

- `src/storage/` -- Backend trait, VolumePool, LocalVolume, RemoteVolume,
  erasure encode/decode, metadata, pathing, volume format, bitrot
- `src/cluster/` -- ClusterManager, identity, placement, topology
- `src/heal/` -- MRF queue, scanner, worker
- `src/multipart/` -- multipart upload state and assembly
- `src/config.rs`, `src/query.rs`
- `src/admin/` -- admin handlers and types (wired through s3s S3Route)
- `src/storage/storage_server.rs` -- internode RPC (wired through s3s S3Route)

## what gets replaced

| file | lines | replacement |
|---|---|---|
| `src/s3/handlers.rs` | 1,695 | `impl S3 for AbixioStore` (~800-1000 lines) |
| `src/s3/auth.rs` | 778 | `impl S3Auth` (~30 lines, delegates to s3s SimpleAuth) |
| `src/s3/errors.rs` | 242 | s3s error types (s3s::s3_error! macro) |
| `src/s3/response.rs` | 389 | s3s smithy-generated DTOs |
| `src/s3/router.rs` | 32 | s3s S3Service as hyper service |

total replaced: ~3,136 lines of protocol code

## what gets async

| file | change |
|---|---|
| `src/storage/mod.rs` | Backend trait: `fn` -> `async fn` |
| `src/storage/mod.rs` | Store trait: `fn` -> `async fn` |
| `src/storage/local_volume.rs` | `std::fs` -> `tokio::fs` |
| `src/storage/remote_volume.rs` | `reqwest::blocking` -> `reqwest` async |
| `src/storage/volume_pool.rs` | async Store impl, parallel shard writes via join_all |
| `src/storage/erasure_encode.rs` | async backend writes, spawn_blocking for reed-solomon CPU work |
| `src/storage/erasure_decode.rs` | async backend reads, spawn_blocking for reed-solomon CPU work |
| `src/multipart/mod.rs` | async part encode/decode |
| `src/heal/worker.rs` | async heal_object |

## architecture after migration

```
HTTP request (hyper)
    |
    v
s3s::S3Service
    |-- S3Route: StorageRoute   (_storage/v1/* -> internode RPC, JWT auth)
    |-- S3Route: AdminRoute     (_admin/* -> admin handlers)
    |-- S3Auth: AbixioAuth      (delegates to s3s SimpleAuth)
    |-- S3Access: AbixioAccess  (cluster fencing check)
    |
    v
s3s::S3 trait  (typed DTOs in, typed DTOs out)
    |
    v
impl S3 for AbixioStore  (glue layer, ~800-1000 lines)
    |-- put_object: extract body + metadata from PutObjectInput, call VolumePool
    |-- get_object: call VolumePool, build GetObjectOutput with body
    |-- ... (41 operations, each a thin async adapter)
    |
    v
VolumePool (async Store trait)
    |
    v
Backend trait (async)
    |-- LocalVolume (tokio::fs)
    |-- RemoteVolume (reqwest async)
```

## per-object EC through s3s

per-object EC uses `x-amz-meta-ec-ftt` headers. s3s passes these through as
`PutObjectInput.metadata` which is `Map<String, String>`. the `ec-ftt` key
arrives with the `x-amz-meta-` prefix stripped per AWS spec. no conflict.

## cluster fencing through s3s

`impl S3Access for AbixioAccess` -- the `check()` method is called before
every operation. return `Err(s3_error!(ServiceUnavailable))` when the cluster
is fenced. clean.

## admin API and internode RPC through s3s

s3s `S3Route` trait intercepts paths before s3s routing:
- `_admin/*` -> existing AdminHandler (unchanged)
- `_storage/v1/*` -> existing StorageServer (unchanged)

both keep their own auth (admin uses S3 auth, internode uses JWT).

## what we get for free

- sigv4 chunked transfer auth (the original motivation)
- sigv4 chunked with trailing checksums
- POST policy uploads
- content-md5 validation
- correct XML serialization for all S3 operations (smithy-generated)
- all S3 error codes
- virtual hosted-style bucket routing
- parallel shard I/O (async backends + join_all)

## implementation phases

### phase 1: async storage layer -- DONE

make Backend and Store async. this is the foundation everything else builds on.

completed 2026-04-06. all 125 tests passing. key changes:
- Backend and Store traits: `fn` -> `async fn` with `#[async_trait]`
- LocalVolume: `std::fs` -> `tokio::fs`
- RemoteVolume: `reqwest::blocking` -> `reqwest` async
- VolumePool: async Store impl
- erasure_encode/decode: async backend calls
- heal worker: async heal_object, removed spawn_blocking
- all tests: `#[test]` -> `#[tokio::test]`

### phase 2: s3s protocol layer -- DONE

replaced hand-rolled S3 protocol code with s3s v0.13.

completed 2026-04-06. deleted 3,137 lines, added 1,243 lines. key files:
- `src/s3_service.rs` -- `impl S3 for AbixioS3` (31 operations, 1086 lines)
- `src/s3_auth.rs` -- `impl S3Auth for AbixioAuth` (single credential pair)
- `src/s3_access.rs` -- `impl S3Access for AbixioAccess` (cluster fencing)
- `src/s3_route.rs` -- `AbixioDispatch` (admin + storage RPC bypass, s3s passthrough)
- deleted: `src/s3/` (handlers.rs, auth.rs, errors.rs, response.rs, router.rs, mod.rs)

design decisions:
- dispatch layer at hyper level (not S3Route) because admin/storage_server
  need hyper::body::Incoming which is incompatible with s3s::Body
- relaxed bucket name validation (s3s enforces 3-63 chars per AWS spec;
  abixio accepts any non-empty name for flexibility)
- no global state (struct owns Arc<VolumePool> + Arc<ClusterManager>)
- single map_err function for consistent error mapping

108/125 integration tests pass. 17 remaining gaps:
- conditional requests (4): not implemented in s3_service.rs
- versioning response headers (5): x-amz-version-id not wired through DTOs
- policy/lifecycle format (4): body format differences
- FTT validation (2): metadata passthrough not propagating errors
- cluster fencing in tests (2): test harness wiring

### phase 3: cleanup -- DONE

completed 2026-04-06:
- removed `pub mod s3` from lib.rs
- updated architecture.md project structure
- updated s3-compliance.md auth + response quality sections
- updated todo.md with completed items
- updated README test count

## risks

| risk | mitigation |
|---|---|
| s3s single maintainer | fork if needed (apache 2.0) |
| async conversion breaks tests | phase 1 is isolated, test before phase 2 |
| s3s body types differ from Vec<u8> | s3s Body has into_bytes(), straightforward adapter |
| admin/internode routes need to co-exist | S3Route trait designed for exactly this |
| reed-solomon is CPU-bound | spawn_blocking for encode/decode, async for I/O |
| doing both changes at once | phased approach, each phase independently testable |
