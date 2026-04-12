# Benchmark Inventory

Complete catalog of every benchmark test across the abixio and abixio-ui
repos. Each entry lists what it measures, where it lives, how to run it,
and what it overlaps with.

Last updated: 2026-04-12.

---

## Overview

| # | Test | Repo | Type | Purpose |
|---|---|---|---|---|
| 1 | `bench_layer_1_primitives` | abixio | in-process | raw filesystem primitives |
| 2 | `bench_layer_2_storage` | abixio | in-process | VolumePool.put_object, no HTTP |
| 3 | `bench_layer_3_http_transport` | abixio | in-process | hyper HTTP round-trip, no S3 |
| 4 | `bench_layer_4_s3s_protocol` | abixio | in-process | s3s SigV4+XML, no real storage |
| 5 | `bench_layer_5_full_client` | abixio | stub | placeholder (points to abixio-ui) |
| 6 | `bench_pool_l0_primitive` | abixio | in-process | WriteSlotPool acquire/release |
| 7 | `bench_pool_l1_slot_write` | abixio | in-process | slot file writes, 6 strategies |
| 8 | `bench_pool_l1_5_json_serializers` | abixio | in-process | JSON serializer shootout |
| 9 | `bench_pool_l2_worker_drain` | abixio | in-process | rename worker drain rate |
| 10 | `bench_pool_l3_integrated_put` | abixio | in-process | write_shard with pool enabled |
| 11 | `bench_pool_l3_5_integration_breakdown` | abixio | in-process | per-step overhead attribution |
| 12 | `bench_pool_l3_6_write_shard_breakdown` | abixio | in-process | inlined write_shard section timing |
| 13 | `bench_perf` | abixio | in-process | layered breakdown L1-L7, JSON output |
| 14 | `bench_pool_l4_tier_matrix` | abixio | in-process | 3-tier x 5-size end-to-end |
| 15 | `bench_pool_l4_5_stack_breakdown` | abixio | in-process | 5-stage latency attribution at 4KB |
| 16 | `bench_0_raw_disk` | abixio-ui | child process | raw tokio::fs baseline |
| 17 | `bench_1_disk` | abixio-ui | child process | AbixIO 1 disk, all ops |
| 18 | `bench_2_disks` | abixio-ui | child process | AbixIO 2 disks (EC 1+1) |
| 19 | `bench_3_disks` | abixio-ui | child process | AbixIO 3 disks (EC 2+1) |
| 20 | `bench_4_disks` | abixio-ui | child process | AbixIO 4 disks (EC 3+1) |
| 21 | `bench_competitive` | abixio-ui | child process | AbixIO vs RustFS vs MinIO |
| 22 | `bench_clients` | abixio-ui | child process | aws-sdk vs aws-cli vs rclone |
| 23 | `bench_matrix` | abixio-ui | child process | 3 servers x 3 clients x 3 sizes x 3 tiers |
| 24 | `bench_4kb.py` | abixio | script | quick 4KB smoke test |
| 25 | `compare_bench.sh` | abixio | script | mc-based 3-way comparison |

---

## Detailed entries

### 1. bench_layer_1_primitives

- **File:** `abixio/tests/layer_bench.rs:68`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_layer_1_primitives`
- **What it measures:** raw `tokio::fs::write` (page cache) vs `write+fsync` (physical disk) at 10MB, 20 iterations.
- **Optimizations tested:** none (filesystem floor)
- **Write cache:** no
- **Overlaps with:** `bench_perf` L3 (disk I/O layer), `bench_0_raw_disk`

### 2. bench_layer_2_storage

- **File:** `abixio/tests/layer_bench.rs:158`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_layer_2_storage`
- **What it measures:** VolumePool.put_object + get_object directly, no HTTP. 10MB, 1 disk, 20 iterations.
- **Optimizations tested:** file tier only (no log/pool/write_cache)
- **Write cache:** no
- **Overlaps with:** `bench_perf` L4 (storage pipeline)

### 3. bench_layer_3_http_transport

- **File:** `abixio/tests/layer_bench.rs:215`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_layer_3_http_transport`
- **What it measures:** reqwest PUT through hyper to a bare handler that reads the body and returns 200. No S3 protocol, no SigV4, no storage. Measures HTTP floor.
- **Optimizations tested:** none (transport floor)
- **Write cache:** no
- **Overlaps with:** `bench_perf` L5 (HTTP transport)

### 4. bench_layer_4_s3s_protocol

- **File:** `abixio/tests/layer_bench.rs:317`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_layer_4_s3s_protocol`
- **What it measures:** s3s SigV4 + XML parsing overhead. Runs through s3s with a NullBackend that does no I/O. Isolates the protocol layer cost.
- **Optimizations tested:** none (protocol floor)
- **Write cache:** no
- **Overlaps with:** `bench_perf` L6 (s3s pipeline)

### 5. bench_layer_5_full_client

- **File:** `abixio/tests/layer_bench.rs:412`
- **Run:** n/a
- **What it measures:** nothing. Stub that prints a message pointing to abixio-ui.
- **Optimizations tested:** n/a
- **Write cache:** n/a
- **Overlaps with:** everything in abixio-ui

### 6. bench_pool_l0_primitive

- **File:** `abixio/tests/layer_bench.rs:433`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_pool_l0_primitive`
- **What it measures:** WriteSlotPool acquire/release cycles. Init time. Contention under concurrent access. No I/O, no rename worker.
- **Optimizations tested:** write pool slot management only
- **Write cache:** no
- **Origin:** Phase 1 of write-pool development
- **Overlaps with:** none (only test of pool primitive in isolation)

### 7. bench_pool_l1_slot_write

- **File:** `abixio/tests/layer_bench.rs:558`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_pool_l1_slot_write`
- **What it measures:** cost of writing shard bytes + meta JSON to a pre-opened slot file. Six write strategies compared: tokio sequential, tokio concurrent (try_join), sync fs, compact JSON, etc. No rename worker, no integration with LocalVolume.
- **Optimizations tested:** write strategies (#1 concurrent writes, #2 compact JSON, #3 sync I/O)
- **Write cache:** no
- **Origin:** Phase 2 of write-pool development
- **Overlaps with:** partially with `bench_pool_l3_integrated_put`

### 8. bench_pool_l1_5_json_serializers

- **File:** `abixio/tests/layer_bench.rs:818`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_pool_l1_5_json_serializers`
- **What it measures:** serializer shootout: serde_json vs simd-json vs sonic-rs vs nanoserde on the ObjectMetaFile payload. Round-trip parse validation.
- **Optimizations tested:** JSON serialization strategy
- **Write cache:** no
- **Origin:** Phase 2.5 of write-pool development
- **Overlaps with:** none (unique micro-benchmark)

### 9. bench_pool_l2_worker_drain

- **File:** `abixio/tests/layer_bench.rs:970`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_pool_l2_worker_drain`
- **What it measures:** rename worker in isolation. Cold drain at various depths (32/256/1024), with and without pre-created dest dirs, steady-state target rates, parallel worker scaling (1/2/4 workers).
- **Optimizations tested:** rename worker throughput, mkdir cost, worker parallelism
- **Write cache:** no
- **Origin:** Phase 3 of write-pool development
- **Overlaps with:** none (only test of rename worker in isolation)

### 10. bench_pool_l3_integrated_put

- **File:** `abixio/tests/layer_bench.rs:1254`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_pool_l3_integrated_put`
- **What it measures:** LocalVolume::write_shard with pool enabled vs file tier (no pool). Multiple sizes. Includes the rename worker running in background.
- **Optimizations tested:** write pool (integrated)
- **Write cache:** no
- **Origin:** Phase 4 of write-pool development
- **Overlaps with:** `bench_pool_l4_tier_matrix` (which adds HTTP stack on top)

### 11. bench_pool_l3_5_integration_breakdown

- **File:** `abixio/tests/layer_bench.rs:1404`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_pool_l3_5_integration_breakdown`
- **What it measures:** per-step timing within LocalVolume::write_shard to find where 39us of integration overhead lives (Phase 4 measured 43us total vs 4us bare write).
- **Optimizations tested:** write pool overhead attribution
- **Write cache:** no
- **Origin:** Phase 4.5 of write-pool development
- **Overlaps with:** `bench_pool_l3_6_write_shard_breakdown` (successor)

### 12. bench_pool_l3_6_write_shard_breakdown

- **File:** `abixio/tests/layer_bench.rs:1613`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_pool_l3_6_write_shard_breakdown`
- **What it measures:** inlined copy of write_shard's pool branch with Instant markers between every section. Reports per-section avg/p50/p99 plus sum vs measured total to find hidden cost.
- **Optimizations tested:** write pool overhead attribution (refined)
- **Write cache:** no
- **Origin:** Phase 5.5+ of write-pool development
- **Overlaps with:** `bench_pool_l3_5_integration_breakdown` (predecessor)

### 13. bench_perf

- **File:** `abixio/tests/layer_bench.rs:1981`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_perf`
- **What it measures:** configurable layered breakdown with JSON output and baseline comparison. Layers:
  - L1: hashing (blake3, md5)
  - L2: RS encode (reed-solomon 3+1)
  - L3: disk I/O (tokio::fs write/read)
  - L4: storage pipeline (VolumePool put_stream/get/get_stream, 1 and 4 disks)
  - L5: HTTP transport (hyper PUT/GET, no S3)
  - L5X: TCP GET variants (raw, writev, bigbuf, stream, all_opts)
  - L6: s3s pipeline (PUT/GET through s3s + AbixioS3, no auth)
  - L6A: s3s + auth (SigV4 signing)
  - L7: full aws-sdk-s3 client (SDK PUT/GET)
  - EXP: experimental GET/PUT variants (chunked, chan_pipe, dbl_buf)
- **Sizes:** configurable via BENCH_SIZES env var (default 4KB, 10MB, 1GB)
- **Optimizations tested:** file tier only (no log/pool/write_cache)
- **Write cache:** no
- **Overlaps with:** supersedes bench_layer_1 through bench_layer_5 (same layers, better infrastructure)

### 14. bench_pool_l4_tier_matrix

- **File:** `abixio/tests/layer_bench.rs:3164`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_pool_l4_tier_matrix`
- **What it measures:** 3 tiers (file/log/pool) x 5 sizes (4KB/64KB/1MB/10MB/100MB), full HTTP stack (reqwest through hyper through s3s through VolumePool). In-process server, sequential PUT then GET, percentile latency + throughput. Comparison tables show pool-vs-file and pool-vs-log speedup.
- **Optimizations tested:** file, log, pool (individually, without write_cache)
- **Write cache:** no
- **Overlaps with:** `bench_matrix` in abixio-ui (same tier comparison but with write_cache always on, plus competitors and multiple clients)

### 15. bench_pool_l4_5_stack_breakdown

- **File:** `abixio/tests/layer_bench.rs:3499`
- **Run:** `k3sc cargo-lock test --release --test layer_bench -- --ignored --nocapture bench_pool_l4_5_stack_breakdown`
- **What it measures:** 5 progressive stages at 4KB PUT (1000 iters):
  - Stage A: hyper bare (HTTP floor)
  - Stage B: hyper + body read + direct write_shard (no s3s)
  - Stage C: full stack + NullBackend (s3s/AbixioS3/VolumePool overhead, zero storage)
  - Stage D: full stack + file tier (real storage)
  - Stage E: full stack + pool tier (depth 32)
  Subtracting consecutive stages attributes latency to each layer.
- **Optimizations tested:** file, pool
- **Write cache:** no
- **Overlaps with:** `bench_perf` (different methodology but similar goal)

### 16. bench_0_raw_disk

- **File:** `abixio-ui/tests/bench.rs:608`
- **Run:** `k3sc cargo-lock test --release --test bench --manifest-path C:\code\abixio-ui\Cargo.toml -- --ignored --nocapture bench_0_raw_disk`
- **What it measures:** raw tokio::fs::write at 1KB/1MB/10MB. Filesystem floor for abixio-ui benchmarks.
- **Optimizations tested:** none
- **Write cache:** no
- **Overlaps with:** `bench_layer_1_primitives`, `bench_perf` L3

### 17-20. bench_1_disk through bench_4_disks

- **File:** `abixio-ui/tests/bench.rs:615-634`
- **Run:** `k3sc cargo-lock test --release --test bench --manifest-path C:\code\abixio-ui\Cargo.toml -- --ignored --nocapture bench_1_disk`
- **What it measures:** full AbixIO server (child process) with 1/2/3/4 disks. PUT, GET, HEAD, DELETE, LIST at multiple sizes (4KB/1KB/1MB/10MB). aws-sdk-s3 client. Shows EC scaling (1+0, 1+1, 2+1, 3+1).
- **Optimizations tested:** default (write_cache ON, default write_tier)
- **Write cache:** yes (always on via main.rs)
- **Overlaps with:** `bench_matrix` (1-disk case is a subset)

### 21. bench_competitive

- **File:** `abixio-ui/tests/bench.rs:877`
- **Run:** `k3sc cargo-lock test --release --test bench --manifest-path C:\code\abixio-ui\Cargo.toml -- --ignored --nocapture bench_competitive`
- **What it measures:** AbixIO vs RustFS vs MinIO at 4KB and 10MB via aws-sdk-s3 only. HTTPS + SigV4.
- **Optimizations tested:** default (write_cache ON, default write_tier)
- **Write cache:** yes (always on via main.rs)
- **Overlaps with:** `bench_matrix` (strict subset -- same servers, one client instead of three, two sizes instead of three)

### 22. bench_clients

- **File:** `abixio-ui/tests/bench.rs:1137`
- **Run:** `k3sc cargo-lock test --release --test bench --manifest-path C:\code\abixio-ui\Cargo.toml -- --ignored --nocapture bench_clients`
- **What it measures:** aws-sdk-s3 vs aws-cli vs rclone against AbixIO at 4KB. HTTPS + SigV4 + UNSIGNED-PAYLOAD. Measures per-client overhead.
- **Optimizations tested:** default (write_cache ON, default write_tier)
- **Write cache:** yes (always on via main.rs)
- **Overlaps with:** `bench_matrix` (strict subset -- one server instead of five, same three clients, one size instead of three)

### 23. bench_matrix

- **File:** `abixio-ui/tests/bench.rs:1620`
- **Run:** `k3sc cargo-lock test --release --test bench --manifest-path C:\code\abixio-ui\Cargo.toml -- --ignored --nocapture bench_matrix`
- **What it measures:** canonical end-to-end comparison. 5 server configs (AbixIO-file, AbixIO-log, AbixIO-pool, RustFS, MinIO) x 3 clients (aws-sdk-s3, aws-cli, rclone) x 3 sizes (4KB, 10MB, 1GB) x 2 ops (PUT, GET). HTTPS + SigV4 + UNSIGNED-PAYLOAD.
- **Optimizations tested:** file, log, pool tiers via --write-tier flag
- **Write cache:** yes (always on via main.rs -- no flag to disable it)
- **Overlaps with:** supersedes `bench_competitive` and `bench_clients`

### 24. bench_4kb.py

- **File:** `abixio/tests/bench_4kb.py`
- **Run:** `python tests/bench_4kb.py --all`
- **What it measures:** 4KB sequential PUT/GET/HEAD/DELETE via Python requests. 1000 ops per test. Can target any server on any port.
- **Optimizations tested:** whatever the server is running
- **Write cache:** depends on server config
- **Overlaps with:** `bench_matrix` 4KB rows, `bench_competitive` 4KB rows

### 25. compare_bench.sh

- **File:** `abixio/tests/compare_bench.sh`
- **Run:** `ABIXIO_BIN=./target/release/abixio RUSTFS_BIN=rustfs MINIO_BIN=minio bash tests/compare_bench.sh`
- **What it measures:** 3-way comparison (AbixIO vs RustFS vs MinIO) via mc CLI. PUT/GET at 1KB/1MB/10MB/100MB, HEAD, LIST (100 objects), DELETE. 5 iterations each.
- **Optimizations tested:** default (write_cache ON via main.rs)
- **Write cache:** yes
- **Overlaps with:** `bench_matrix` (similar coverage but via mc instead of aws-sdk/cli/rclone)

---

## Overlap map

What each test uniquely contributes vs what is duplicated.

### Duplicated coverage

| Measurement | Tested by |
|---|---|
| Raw disk I/O baseline | `bench_layer_1_primitives`, `bench_perf` L3, `bench_0_raw_disk` |
| VolumePool without HTTP | `bench_layer_2_storage`, `bench_perf` L4 |
| HTTP transport floor | `bench_layer_3_http_transport`, `bench_perf` L5 |
| s3s protocol overhead | `bench_layer_4_s3s_protocol`, `bench_perf` L6 |
| Full SDK client | `bench_perf` L7, `bench_1_disk` |
| 3-tier comparison (file/log/pool) | `bench_pool_l4_tier_matrix`, `bench_matrix` |
| AbixIO vs competitors | `bench_competitive`, `bench_matrix`, `compare_bench.sh`, `bench_4kb.py` |
| Client comparison | `bench_clients`, `bench_matrix` |
| Pool overhead breakdown | `bench_pool_l3_5_integration_breakdown`, `bench_pool_l3_6_write_shard_breakdown` |

### Unique coverage (only one test provides this)

| Measurement | Only in |
|---|---|
| WriteSlotPool acquire/release primitive | `bench_pool_l0_primitive` |
| Slot write strategy comparison (6 strategies) | `bench_pool_l1_slot_write` |
| JSON serializer shootout | `bench_pool_l1_5_json_serializers` |
| Rename worker drain rate + parallel scaling | `bench_pool_l2_worker_drain` |
| write_shard pool vs file at storage layer | `bench_pool_l3_integrated_put` |
| 5-stage stack attribution (A through E) | `bench_pool_l4_5_stack_breakdown` |
| Multi-disk EC scaling (2/3/4 disks) | `bench_2_disks` through `bench_4_disks` |
| Experimental GET/PUT variants | `bench_perf` EXP layer |
| mc client comparison | `compare_bench.sh` |

---

## Known issues

1. **Write cache is always on in child-process benchmarks.** `main.rs:155`
   unconditionally calls `enable_write_cache(256MB)`. There is no
   `--no-write-cache` CLI flag. Every abixio-ui benchmark (bench_matrix,
   bench_competitive, bench_clients, bench_1_disk through bench_4_disks)
   has write_cache enabled. This means the tier comparison in bench_matrix
   is actually testing file+wc, log+wc, pool+wc -- not the raw tiers.

2. **No benchmark tests write_cache in isolation.** There is no test that
   measures the write_cache benefit by comparing with-cache vs
   without-cache for the same tier.

3. **bench_pool_l4_tier_matrix and bench_matrix measure the same thing
   differently.** The layer_bench version runs in-process without
   write_cache. The abixio-ui version spawns child processes with
   write_cache on. Results are not directly comparable.

4. **bench_competitive and bench_clients are strict subsets of
   bench_matrix.** They exist as lighter-weight standalone runs but
   measure nothing bench_matrix does not.

5. **bench_layer_1 through bench_layer_5 are superseded by bench_perf.**
   bench_perf covers the same layers with better infrastructure (JSON
   output, configurable sizes/layers, baseline comparison).

6. **Pool development benchmarks (L0 through L3.6) are frozen in time.**
   These 7 tests were built incrementally during write-pool development.
   The pool code is stable. They still run and pass but their primary
   purpose (guiding development) is complete.

7. **bench_layer_5_full_client is a stub.** It prints a message and does
   nothing.
