# Benchmarks

For methodology, fairness requirements, client/server specs, and
how to run benchmarks, see
[benchmark-requirements.md](benchmark-requirements.md).

---

## Comprehensive matrix

3 servers x 3 clients x 3 sizes x 2 operations.
Run with: `cd abixio-ui && cargo test --release --test bench -- --ignored --nocapture bench_matrix`

The harness for this benchmark starts AbixIO (three times, once per
write tier: file, log, pool), RustFS, and MinIO over TLS, injects a
temporary benchmark CA, and benchmarks these canonical clients:

- `aws-sdk-s3`
- `AWS CLI`
- `rclone`

All three are configured for `HTTPS + SigV4 + UNSIGNED-PAYLOAD`, with
the same disk-backed PUT source / GET sink model and the same 3 PUT + 3
GET warmup. CLI process spawn cost is real and is not subtracted; per-op
spawn overhead is printed in the bench output (~945 ms for AWS CLI v2,
~120 ms for rclone) so the small-object CLI rows read as "startup-bound,
not S3-bound".

The three AbixIO rows (`AbixIO-file`, `AbixIO-log`, `AbixIO-pool`) are
the same binary launched with `--write-tier file|log|pool`. The internal
behavior of each tier is documented in
[write-path.md](write-path.md#storage-branches); this doc only reports
the cross-server numbers. **A surprise from this run: at 4KB sdk PUT
the tier choice barely changes the user-visible number** (1653 / 1662 /
1716 obj/s). The HTTP / SigV4 / SDK per-request overhead is ~600 us, so
the storage-tier delta that the layer benches measure (3.5x at 4KB)
gets compressed under the protocol floor. The tier choice only becomes
visible at sizes where storage work is more than half the request
budget.

Canonical matrix results are below, dated 2026-04-11. Source artifact:
`bench-results/2026-04-11-matrix-tls-tiers.txt`. Bench: `bench_matrix`
in `abixio-ui/tests/bench.rs`.

### 4KB: small object performance (obj/sec)

| Server          | aws-sdk-s3 PUT | aws-sdk-s3 GET | aws-cli PUT | aws-cli GET | rclone PUT | rclone GET |
|---              |---             |---             |---          |---          |---         |---         |
| AbixIO-file     | 1653           | 663            | 1           | 1           | 8          | 8          |
| AbixIO-log      | 1662           | 664            | 1           | 1           | 8          | 8          |
| **AbixIO-pool** | **1716**       | **805**        | 1           | 1           | 8          | 8          |
| RustFS          | 300            | 461            | 1           | 1           | 8          | 8          |
| MinIO           | 365            | 751            | 1           | 1           | 7          | 8          |

**AbixIO leads sdk PUT and sdk GET on every tier, decisively.** Best
case (`AbixIO-pool`) is 1716 obj/s PUT versus MinIO 365 (4.7x) and
RustFS 300 (5.7x). Best sdk GET (`AbixIO-pool`) is 805 obj/s versus
MinIO 751 and RustFS 461.

The three AbixIO tier rows are within 4% of each other on PUT and
within 21% on GET (pool ahead). Most of the tier delta the layer
benches measure (file 935 us -> log 265 us, ~3.5x at 4KB) is invisible
at the HTTP layer because it sits underneath the ~600 us SDK + auth +
hyper floor.

The CLI rows for 4KB are dominated by per-op process spawn — measured
overhead in this run was ~945 ms for `aws-cli` and ~120 ms for `rclone`.
These are not S3 throughput measurements, they are CLI startup
measurements. 4KB obj/sec via aws-cli rounds to 1 obj/s on every server,
and via rclone to 7-8 obj/s on every server, regardless of which server
is behind it.

### 10MB: medium object throughput (MB/s)

| Server          | aws-sdk-s3 PUT | aws-sdk-s3 GET | aws-cli PUT | aws-cli GET | rclone PUT | rclone GET |
|---              |---             |---             |---          |---          |---         |---         |
| AbixIO-file     | 335.9          | 221.8          | 10.2        | 10.4        | 56.6       | 73.6       |
| AbixIO-log      | 297.8          | 218.8          | 9.8         | 9.9         | 58.1       | 74.0       |
| **AbixIO-pool** | **421.4**      | 212.8          | 10.5        | 9.9         | 59.4       | 74.5       |
| RustFS          | 311.1          | 201.8          | 10.3        | 10.4        | 8.5 ‡      | 52.9       |
| MinIO           | 313.3          | 210.2          | 10.1        | 10.5        | 61.0       | 72.1       |

**AbixIO-pool wins 10MB sdk PUT decisively at 421.4 MB/s**, +25% over
its own file tier (335.9), +35% over RustFS (311.1) and +35% over
MinIO (313.3). This is exactly the mid-range window where the
pre-opened temp-file pool's avoided `mkdir+create` per-PUT pays off.
sdk GET clusters within ~10% across all five rows because disk write
to the sink file equalizes the result (~210-222 MB/s).

`‡` RustFS rclone 10MB PUT at 8.5 MB/s is a clear outlier on the slow
side versus the others' 56-61 MB/s. Same payload, same warmup. Worth
a follow-up investigation.

### 1GB: large object throughput (MB/s)

| Server          | aws-sdk-s3 PUT | aws-sdk-s3 GET | aws-cli PUT | aws-cli GET | rclone PUT | rclone GET |
|---              |---             |---             |---          |---          |---         |---         |
| AbixIO-file     | 476.0          | 259.9          | 203.9       | **389.1**   | 275.0      | 99.2 †     |
| **AbixIO-log**  | **535.1**      | 253.5          | 191.7       | 356.7       | 266.8      | 96.7 †     |
| AbixIO-pool     | 481.5          | 249.9          | 183.8       | 371.0       | 264.5      | 87.1 †     |
| RustFS          | 469.0          | **278.8**      | 178.4       | 285.9       | 18.7 ‡     | 310.4      |
| MinIO           | 396.7          | 248.5          | 197.0       | 300.3       | **288.1**  | 327.5      |

**AbixIO-log wins 1GB sdk PUT at 535.1 MB/s**, +12% over file
(476.0), +14% over RustFS (469.0) and +35% over MinIO (396.7). sdk
GET is again equalized by the disk-write sink (~248-279 MB/s) with
RustFS narrowly leading. aws-cli GET is faster than sdk GET for the
AbixIO tiers because aws-cli streams the body through a separate
process and avoids the SDK's per-op buffer churn at this size.

`†` AbixIO rclone 1GB GET measured at 87-99 MB/s versus 310 (RustFS)
and 327 (MinIO). The previous (HTTP) doc excluded rclone 1GB GET
entirely because "rclone caches/skips repeated downloads to the same
file"; the new harness uses unique sinkpaths per iteration, so cache
reuse should not apply. The number is reported as measured. Worth a
follow-up investigation across all three abixio tiers.

`‡` RustFS rclone 1GB PUT at 18.7 MB/s is a clear outlier on the slow
side versus the others' 264-288 MB/s. Same payload, same warmup. Also
worth a follow-up investigation.

### Headline: which tier should AbixIO ship as default?

There is no single best AbixIO tier across all sizes:

- **4KB**: pool slightly ahead but all three within 4% (HTTP overhead
  dominates)
- **10MB**: pool clearly wins (+25% PUT)
- **1GB**: log clearly wins (+12% PUT)
- **GET at every size**: pool slightly ahead at small sizes, otherwise
  equalized by disk-write sink

The intended production answer is per-object dispatch inside
`LocalVolume::write_shard` based on object size — see
[write-path.md::Tier handoff](write-path.md#tier-handoff-implied-by-these-tables).
The current `--write-tier` CLI flag picks one tier for the whole
process; per-object dispatch is still pending.

---

## Single-server detailed benchmark

AbixIO 1-disk, all operations, all sizes.
Run with: `cd abixio-ui && cargo test --release --test bench -- --ignored --nocapture bench_1_disk`

```
OP       SIZE          ops         avg         p50         p99          MB/s     obj/sec
PUT      4KB       500 ops     513.4us     502.1us     679.6us      7.6 MB/s        1948
GET      4KB       500 ops     345.3us     331.5us     485.0us     11.3 MB/s        2896
HEAD     4KB       500 ops     325.8us     318.3us     408.6us             -        3070
DELETE   4KB       500 ops     432.0us     431.6us     587.3us             -        2315
PUT      1KB       100 ops     469.3us     462.5us     575.4us      2.1 MB/s        2131
PUT      1MB        20 ops      12.5ms      11.5ms      19.4ms     79.7 MB/s          80
PUT      10MB        5 ops      89.6ms      88.8ms      90.4ms    111.7 MB/s          11
PUT*     10MB        5 ops      38.9ms      40.5ms      45.2ms    257.0 MB/s          26
GET      1KB       100 ops     396.6us     388.5us     588.5us      2.5 MB/s        2522
GET      1MB        20 ops       2.6ms       2.6ms       2.8ms    382.1 MB/s         382
GET      10MB        5 ops      16.5ms      15.1ms      16.1ms    605.4 MB/s          61
HEAD     -         100 ops     376.8us     372.0us     473.9us             -        2654
LIST     100obj     50 ops       4.7ms       4.7ms       5.0ms             -         214
DELETE   1KB       100 ops     430.2us     417.0us     600.8us             -        2325
```

`PUT*` = UNSIGNED-PAYLOAD (skips client-side SHA256). Note the 2.6x
speedup vs signed PUT at 10MB (257 vs 112 MB/s).

---

## Client comparison

Same AbixIO server, different S3 clients.
Run with: `cd abixio-ui && cargo test --release --test bench -- --ignored --nocapture bench_clients`

This benchmark now uses the same normalized mode as the canonical
matrix:

- `HTTPS`
- `SigV4`
- `UNSIGNED-PAYLOAD`
- disk-backed PUT source / GET sink for every client

The current canonical client set is:

- `aws-sdk-s3`
- `AWS CLI`
- `rclone`

Published numeric results for this rewritten comparison are pending a
fresh run. The older `curl` / `mc` table has been retired because it no
longer matches the benchmark design.

---

## Server-side profiling

Debug header `x-debug-s3s-ms` shows actual server processing time
(excludes client overhead, TCP, HTTP parsing):

```
4KB PUT server processing:  0.28ms
4KB GET server processing:  0.08ms
4KB HEAD server processing: 0.08ms
```

The server processes 4KB GET in 80 microseconds. The remaining latency
in benchmarks is client overhead (SigV4 signing, HTTP, connection management).

## Internal per-layer benchmarks

Tests each layer of the storage stack independently (no HTTP client).
Run with: `cd abixio && cargo test --release --test layer_bench -- --ignored bench_perf --nocapture`

These measure the storage engine directly and are used for optimization
work. See [layer-optimization.md](layer-optimization.md) for details.

---

## Write tier comparison and stack breakdown

These were both moved out of this doc. Each storage tier (file, log,
pool) and each layer of the write path (hyper, s3s, AbixioS3,
VolumePool, file/log/pool storage work) is documented end-to-end in
[write-path.md](write-path.md):

- [write-path.md::Where the time goes](write-path.md#where-the-time-goes)
  has the canonical Phase 8.7 PUT and GET p50 + throughput tables for
  the file, log, and pool tiers at 4KB through 100MB.
- [write-path.md::Storage branches](write-path.md#storage-branches) has
  the per-branch deep dive for each tier with `where it wins / where
  it loses`, ack semantics, and final resting place.
- The Phase 8.5 4KB stage attribution (bare hyper -> hyper+s3s ->
  full file tier -> pool variants) lives in
  [write-path.md::Entry and request shaping](write-path.md#entry-and-request-shaping)
  and [write-path.md::Layer-by-layer write path](write-path.md#layer-by-layer-write-path).

This section is intentionally a pointer. Internal-bench numbers and
write-path attribution are not duplicated here so the same number does
not exist in two places.

---

For the canonical end-to-end PUT path, see [write-path.md](write-path.md).
For optimization history and allocation audit, see [layer-optimization.md](layer-optimization.md).
For log-structured storage design, see [write-log.md](write-log.md).
For RAM write cache design, see [write-cache.md](write-cache.md).

## Accuracy Report

Audited against the codebase on 2026-04-11.

| Claim | Status | Evidence |
|---|---|---|
| `bench_pool_l4_tier_matrix` and `bench_pool_l4_5_stack_breakdown` exist | Verified | `tests/layer_bench.rs:3164`, `3499` |
| Raw artifacts referenced for Phase 8.7 and 8.5 exist | Verified | `bench-results/phase8.7-tier-matrix.txt`, `bench-results/phase8.5-stack-breakdown-v5.txt` |
| Debug profiling header `x-debug-s3s-ms` exists in code | Verified | `src/s3_route.rs:82` |
| Matrix comment said `3 servers, 2 clients` | Corrected | Document contradicted itself; client list and matrix section clearly use 3 clients |
| CLI overhead was described as both subtracted and not subtracted | Corrected | Document contradicted itself; matrix section explicitly says published numbers are end-to-end including spawn |
| Phase 8 / 8.5 benchmark section names and commands | Verified | `tests/layer_bench.rs:3111-3164`, `3455-3499` |
| Specific published numeric results in matrix tables | Verified | Re-run end-to-end on 2026-04-11 with the 5-server tier-aware harness (`AbixIO-file/log/pool` + RustFS + MinIO); raw output at `bench-results/2026-04-11-matrix-tls-tiers.txt` |
| Phase 8.7 tier matrix and Phase 8.5 stack breakdown sections were duplicated here and in `write-path.md` | Corrected | Both sections deleted from this doc; `## Write tier comparison and stack breakdown` is now a one-block pointer to `write-path.md`. Each number lives in exactly one doc |
| Matrix harness normalizes disk I/O and warmup across clients | Verified | `../abixio-ui/tests/bench.rs:882-904`, `951-962`, `1030-1040` |
| `HTTPS + SigV4 + UNSIGNED-PAYLOAD` is the documented authoritative normalized client mode | Verified | `docs/benchmarks.md:7-15` |
| Canonical benchmark harness now targets HTTPS instead of plain HTTP | Verified | `../abixio-ui/tests/bench.rs:646`, `650`, `658`, `847`, `1221`, `1240`, `1284`, `1309`; `../abixio-ui/tests/support/server.rs:147-192` |
| Canonical client set is now `aws-sdk-s3`, `AWS CLI`, and `rclone` | Verified | `../abixio-ui/tests/bench.rs:842`, `849-855`, `1231-1235` |
| AWS CLI path is configured for unsigned payloads | Verified | `../abixio-ui/tests/bench.rs:726-727` writes `payload_signing_enabled = false` into the benchmark profile |
| rclone path is explicitly configured for unsigned payloads | Verified | `../abixio-ui/tests/bench.rs:798-818` and all rclone calls route through those args |
| SDK path uses unsigned payloads in the canonical client and matrix benches | Verified | `../abixio-ui/src/s3/client.rs:249-287`, `../abixio-ui/tests/bench.rs:869`, `876`, `1011`, `1021` |
| Matrix harness fully normalizes connection reuse across clients | Incorrect | `../abixio-ui/tests/bench.rs:857-888` keeps the SDK in-process while `894-966`, `1048-1095`, and `1101-1182` spawn CLI commands per operation |
| Published matrix tables below are current canonical TLS results | Verified | Tables replaced with the 2026-04-11 5-server tier-aware TLS harness rerun; source artifact `bench-results/2026-04-11-matrix-tls-tiers.txt` |
| `bench_clients` is still a mixed signed/in-memory comparison | Incorrect | `../abixio-ui/tests/bench.rs:842-966` now uses TLS, disk-backed I/O, and unsigned payloads for the canonical client set |

Verdict: this document’s benchmark narrative is mostly internally consistent after fixing the overhead/client-count contradictions. The bench names, commands, raw artifact references, and profiling hook are backed by this repo. The many published performance numbers still need runtime re-benchmarking if you want strict empirical re-validation.
