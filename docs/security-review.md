# RustFS Security Review

This review was done against a fresh clone of `rustfs/rustfs` in `C:\code\rustfs` on April 5, 2026.

It focused on the exact claims that were circulating:

- hardcoded or dangerously defaulted secrets
- path traversal in bucket/object/version/upload paths
- unsanitized storage-server query handling

This is a source-backed review, not a repost of social media claims.

## Findings

### 1. AbixIO has real traversal bugs right now

Severity: Critical

This is the immediate problem, regardless of what RustFS did.

AbixIO currently joins attacker-controlled bucket, key, and version values directly into local filesystem paths without rejecting `..`, absolute paths, or hostile segments.

Confirmed examples:

- `src/storage/local_volume.rs:76`
- `src/storage/local_volume.rs:100`
- `src/storage/local_volume.rs:126`
- `src/storage/local_volume.rs:162`
- `src/storage/local_volume.rs:233`
- `src/storage/local_volume.rs:264`
- `src/storage/local_volume.rs:285`
- `src/storage/local_volume.rs:333`
- `src/storage/local_volume.rs:346`
- `src/multipart/mod.rs:381`

AbixIO also percent-decodes query values before using them:

- `src/query.rs:3`

And the storage server accepts decoded `bucket`, `key`, and `version_id` directly from query params:

- `src/storage/storage_server.rs:107`
- `src/storage/storage_server.rs:129`
- `src/storage/storage_server.rs:149`
- `src/storage/storage_server.rs:296`
- `src/storage/storage_server.rs:319`
- `src/storage/storage_server.rs:340`

There is even an existing test that confirms encoded bucket/key values are accepted:

- `tests/admin_integration.rs:661`

Nested object keys are also intentionally supported today:

- `src/storage/local_volume.rs:517`

That means AbixIO needs a real object-name sanitization layer, not a blanket "no slashes" rule.

### 2. RustFS does not appear to have the same raw path-join bug anymore

Severity: Important positive finding

RustFS has two defense layers that AbixIO currently lacks:

Object and prefix validation rejects bad path components:

- `crates/ecstore/src/bucket/utils.rs:170`
- `crates/ecstore/src/bucket/utils.rs:190`
- `crates/ecstore/src/bucket/utils.rs:315`
- tests at `crates/ecstore/src/bucket/utils.rs:383`
- tests at `crates/ecstore/src/bucket/utils.rs:428`

Local disk access is forced through a containment check under the disk root:

- `crates/ecstore/src/disk/local.rs:411`
- `crates/ecstore/src/disk/local.rs:429`
- `crates/ecstore/src/disk/local.rs:440`
- `crates/ecstore/src/disk/local.rs:1239`

The important distinction is that RustFS does not just "clean" paths. It also rejects bad object names before they hit storage code, and the local disk layer checks that normalized paths still stay under the configured root.

That is materially stronger than AbixIO's current approach.

### 3. RustFS version ID handling is stronger than AbixIO's

Severity: Important positive finding

RustFS validates `versionId` as UUID-shaped input before using it:

- `rustfs/src/storage/options.rs:69`
- `rustfs/src/storage/options.rs:123`
- `rustfs/src/storage/options.rs:198`

AbixIO currently accepts and forwards raw `versionId` / `version_id` values:

- `src/s3/handlers.rs:545`
- `src/s3/handlers.rs:650`
- `src/storage/storage_server.rs:298`
- `src/storage/storage_server.rs:321`
- `src/storage/storage_server.rs:342`

That leaves AbixIO exposed to version-path traversal in addition to normal object-path traversal.

### 4. RustFS multipart upload IDs are validated, but the implementation shape is still worth watching

Severity: Medium

RustFS validates multipart upload IDs as base64-like input before using them:

- `crates/ecstore/src/bucket/utils.rs:270`
- `crates/ecstore/src/bucket/utils.rs:315`

Multipart handling then routes through metadata-derived paths rather than directly joining raw request values into arbitrary filesystem paths:

- `crates/ecstore/src/store/multipart.rs:28`
- `crates/ecstore/src/store/multipart.rs:182`
- `crates/ecstore/src/store/multipart.rs:255`
- `crates/ecstore/src/set_disk/metadata.rs:37`
- `crates/ecstore/src/set_disk/multipart.rs:95`

This is not as clean as "UUID only", but it is still much stronger than AbixIO's current raw `upload_dir(root, bucket, key, upload_id)` pattern.

### 5. RustFS still ships with real insecure default credentials

Severity: High

This part of the criticism is justified.

RustFS has explicit built-in defaults:

- `crates/credentials/src/constants.rs:21`
- `crates/credentials/src/constants.rs:28`

Config resolution falls back to those defaults when credentials are not provided:

- `rustfs/src/config/config_struct.rs:30`
- `rustfs/src/config/config_struct.rs:137`
- `rustfs/src/config/config_struct.rs:138`

Startup warns if defaults are in use, but does not fail closed:

- `rustfs/src/server/http.rs:213`

There are also deployment examples and service files that include `rustfsadmin`:

- `Dockerfile.glibc:97`
- `docker-compose.yml:37`
- `docker-compose-simple.yml:34`
- `deploy/config/rustfs.env:2`
- `deploy/build/rustfs.service:20`

So the "default admin credential" concern is real. The codebase acknowledges it as a warning, not as a hard failure.

### 6. RustFS RPC auth also falls back to the default secret

Severity: High

RustFS internode/RPC signing falls back to the global secret, and then to the built-in default secret if nothing else is configured:

- `crates/credentials/src/credentials.rs:218`
- `crates/ecstore/src/rpc/http_auth.rs:19`
- `crates/ecstore/src/rpc/http_auth.rs:38`

If an operator stands up RustFS with stock defaults, the RPC shared secret is predictable too.

That does not recreate the path traversal issue, but it is still a serious deployment footgun.

### 7. I did not confirm the "static key vibe coded into the product" claim as a current hardcoded secret in the request path

Severity: Clarification

What I did confirm is:

- fixed default admin credentials exist
- fixed default RPC-secret fallback exists
- many deployment examples still embed those values

That is already bad enough. I did not find a separate "hidden magic key" in current request-signing logic beyond those defaults.

## What RustFS Is Doing Better Than AbixIO

- Rejecting object names and prefixes that contain `.` or `..` path components.
- Validating multipart upload IDs before using them.
- Validating version IDs instead of treating them as arbitrary path strings.
- Forcing local object paths through a root-containment check.

AbixIO should copy that model immediately:

1. Validate object keys by path component, not by naive substring checks.
2. Reject `.` and `..` path segments after percent-decoding.
3. Reject absolute paths and Windows drive-qualified paths.
4. Treat `version_id` and `upload_id` as structured identifiers, not free-form path text.
5. Enforce a final containment check after path normalization.

## What AbixIO Is Still Doing Better Conceptually

- Per-object erasure selection is still a real product differentiator.
- Heterogeneous volume/backend design is still directionally different from RustFS.
- A thick native UI that exposes shard layout, repair state, and storage mechanics remains a real differentiator if implemented well.

None of that matters if the storage layer is path-traversable.

## Immediate AbixIO Remediation

Priority order:

1. Add a single shared sanitizer for bucket, key, version ID, and upload ID inputs.
2. Make all filesystem access go through a `safe_join(root, rel)` helper with containment enforcement.
3. Stop accepting empty auth defaults in `src/main.rs:55`.
4. Add regression tests for:
   - `../`
   - `%2e%2e`
   - absolute paths
   - Windows drive letters
   - malicious `versionId`
   - malicious `uploadId`
5. Re-check `RemoteVolume` and storage-server routes after the sanitizer lands.

## Validation Notes

- This review is source-based and limited to the current checked RustFS tree at `C:\code\rustfs`.
- I did not rely on social posts as evidence.
- I attempted targeted RustFS tests, but full targeted `cargo test` invocations timed out locally during build, so the findings above are based on code inspection rather than completed test runs.
