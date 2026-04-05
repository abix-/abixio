# RustFS Security Review

This review was done against a fresh clone of `rustfs/rustfs` in `C:\code\rustfs` on April 5, 2026.

It focused on the exact claims that were circulating:

- hardcoded or dangerously defaulted secrets
- path traversal in bucket/object/version/upload paths
- unsanitized storage-server query handling

This is a source-backed review, not a repost of social media claims.

## Current Status

The biggest AbixIO issue identified during this review was real: path traversal risk in local storage, multipart paths, and internode query handling.

That issue has now been fixed in AbixIO by commit `9e25e79` (`Harden storage path handling`).

The fix added:

- shared validation and safe path construction in `src/storage/pathing.rs`
- explicit invalid-input errors in `src/storage/mod.rs`
- defensive validation in local storage, multipart handling, internode storage routes, admin handlers, and remote volume access
- regression coverage in `tests/admin_integration.rs` and `tests/s3_integration.rs`

Safe nested keys such as `a/b/c` still work. Hostile values such as `../`, encoded traversal, invalid `versionId`, and invalid `uploadId` are now rejected with explicit `400`-class errors instead of reaching filesystem path joins.

## Findings

### 1. AbixIO had a real traversal bug, and it is now remediated

Severity at discovery: Critical

The original issue was that AbixIO joined attacker-controlled bucket, key, version, and multipart values into local filesystem paths without a shared sanitizer or final root-containment check.

That was confirmed in the original review across local volume access, multipart storage, and decoded storage-server query handling.

Status now:

- fixed in `src/storage/pathing.rs`
- fixed in `src/storage/local_volume.rs`
- fixed in `src/multipart/mod.rs`
- fixed in `src/storage/storage_server.rs`
- fixed in `src/storage/erasure_set.rs`
- fixed in `src/storage/remote_volume.rs`
- fixed in `src/admin/handlers.rs`
- fixed in `src/heal/worker.rs`

What changed:

- bucket names are validated before filesystem use
- object keys and prefixes are validated by logical path segment
- `versionId` and `uploadId` are treated as structured identifiers
- local filesystem access now goes through validated path builders and root-contained joins
- invalid inputs surface as explicit client errors instead of accidental `404` or `500` paths

Regression tests now cover hostile:

- `../`
- `%2e%2e`
- invalid bucket names
- invalid `versionId`
- invalid `uploadId`

This was the right first emergency fix.

### 2. RustFS does not appear to have the same raw path-join bug anymore

Severity: Important positive finding

RustFS has two defense layers that were stronger than pre-fix AbixIO:

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

The important distinction is that RustFS does not just "clean" paths. It rejects bad object names before they hit storage code, and the local disk layer checks that normalized paths still stay under the configured root.

AbixIO now follows the same general model.

### 3. RustFS version ID handling was stronger than pre-fix AbixIO

Severity: Important positive finding

RustFS validates `versionId` as UUID-shaped input before using it:

- `rustfs/src/storage/options.rs:69`
- `rustfs/src/storage/options.rs:123`
- `rustfs/src/storage/options.rs:198`

That was a meaningful gap in AbixIO during the original review.

Status now:

- AbixIO also validates `versionId` before storage-path use as part of the `9e25e79` hardening pass.

### 4. RustFS multipart upload IDs are validated, and AbixIO now does the same

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

AbixIO previously used a raw `upload_dir(root, bucket, key, upload_id)` pattern.

Status now:

- AbixIO validates multipart upload IDs and hostile upload-path input is covered by regression tests.

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

### 7. I did not confirm the "static key vibe coded into the product" claim as a separate hidden request-path backdoor

Severity: Clarification

What I did confirm is:

- fixed default admin credentials exist
- fixed default RPC-secret fallback exists
- many deployment examples still embed those values

That is already bad enough. I did not find a separate hidden magic key in current request-signing logic beyond those defaults.

## What RustFS Was Doing Better Than Pre-Fix AbixIO

- Rejecting object names and prefixes that contain `.` or `..` path components.
- Validating multipart upload IDs before using them.
- Validating version IDs instead of treating them as arbitrary path strings.
- Forcing local object paths through a root-containment check.

AbixIO has now copied the important parts of that model.

## What AbixIO Still Needs To Review

The traversal fix closed the most urgent gap, but this review still leaves active follow-up work:

1. Review auth bootstrap and reject unsafe default or empty credentials at startup.
2. Audit all future backend implementations against the same validation/pathing rules.
3. Keep coverage for Windows path forms, percent-decoded traversal, and internode query handling.
4. Re-run this review whenever new volume backends or admin inspection features land.

## What AbixIO Is Still Doing Better Conceptually

- Per-object erasure selection is still a real product differentiator.
- Heterogeneous volume/backend design is still directionally different from RustFS.
- Single-node through multi-node deployment under one model is still a differentiator.
- A thick native console that exposes shard layout, repair state, and storage mechanics remains a real differentiator if implemented well.

## Validation Notes

- This review is source-based and limited to the checked RustFS tree at `C:\code\rustfs`.
- I did not rely on social posts as evidence.
- I attempted targeted RustFS tests, but full targeted `cargo test` invocations timed out locally during build, so the findings above are based on code inspection rather than completed test runs.
- The AbixIO remediation status in this document reflects the shipped hardening commit `9e25e79`.
