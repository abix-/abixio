# AbixIO Project State

## Last Session: 2026-04-12 (benchmark consolidation)

### What We Worked On
- Consolidated all benchmarks from two repos (abixio + abixio-ui) into
  a single `abixio-ui bench` subcommand with full CLI flags.
- Wrote benchmark requirements spec (`docs/benchmark-requirements.md`)
  defining what we test, how, and the fairness rules.
- Implemented L1 through L7 benchmark layers in `abixio-ui/src/bench/`.
- Moved pool internal benchmarks (L0-L3.6) to `l3_pool_internals.rs`.
- Moved 5-stage stack attribution to `l6_stack_breakdown.rs`.
- Moved support code (TlsMaterial, AbixioServer, ExternalServer,
  AwsCliHarness, rclone helpers) to `src/bench/{tls,servers,clients}.rs`.
- Added `--write-cache` flag to abixio server (configurable size, 0 disables).
- Added `--tls on/off/both` flag for HTTP vs HTTPS benchmarking.
- Added JSON output (`--output`) and baseline comparison (`--baseline`).
- Added roundtrip verification before L7 benchmarks.
- Added signed + unsigned PUT benchmarks.
- Added disk-backed I/O fairness for SDK client (read from disk, write to disk).
- Deleted all old benchmark files: `tests/layer_bench.rs` (4194 lines),
  `tests/bench_4kb.py`, `tests/compare_bench.sh`,
  `abixio-ui/tests/bench.rs`, `abixio-ui/tests/support/{server,tls}.rs`.
- Updated all doc references across 8 files to point at new locations.
- Merged `benchmark-inventory.md` into `benchmark-requirements.md`.

### Decisions Made
- All benchmarks live in `abixio-ui/src/bench/`. abixio-ui depends on
  abixio as a library so it can call internal APIs for in-process L1-L6
  testing and spawn child processes for L7.
- Write cache is a separate axis from write tier. 3 tiers x 2 cache
  states = 6 AbixIO configurations.
- Benchmark reporting uses p50 (median), p95 (tail), p99 (worst case).
- CLI flags control everything. No env vars.
- Two benchmark docs: `benchmark-requirements.md` (spec) and
  `benchmarks.md` (results).

### Current State
- `abixio-ui bench` subcommand works with all flags. Tested L1-L7.
- abixio has `--write-cache <MB>` flag (default 256, 0 disables).
- All old test/bench files deleted from both repos.
- All docs updated. No stale references.
- The numeric results in `benchmarks.md` were measured on 2026-04-11
  with the old harness. They need re-running with `abixio-ui bench`.

### Next Steps
1. Run `abixio-ui bench` end-to-end and capture fresh results.
2. Update `benchmarks.md` matrix tables with new numbers.
3. Resume pool work: Phase 6 crash recovery, Phase 7 backpressure.
4. Outstanding: log store GC, heal worker log-awareness, v0.1.0 release.

### k3s NodePort Allocation
| Port  | Service   |
|-------|-----------|
| 30080 | Ascender  |
