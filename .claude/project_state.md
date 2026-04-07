# AbixIO Project State

## Last Session: 2026-04-07 (comparative benchmark script)

### What We Worked On
- Created `tests/compare_bench.sh`: head-to-head AbixIO vs RustFS benchmark
- Script starts both servers (single-node, single-disk, tmpdir, no-auth)
- Tests PUT/GET at 1KB/1MB/10MB/100MB + HEAD/LIST/DELETE via `mc`
- Outputs markdown table with MB/s and avg latency
- Updated `docs/benchmarks.md` with "vs RustFS" section and run instructions

### Decisions Made
- **mc client, not Rust aws-sdk-s3:** eliminates client variance, both servers
  get identical UNSIGNED-PAYLOAD requests. reproducible by anyone with binaries
- **shell script, not Criterion:** no cargo dependency, just two servers + mc
- **sequential requests only:** concurrent/parallel is phase 2

### Current State
- 171 tests passing, 41/72 S3 operations
- Layer benchmarks: 147 MB/s PUT, 249 MB/s GET (10MB, 1 disk)
- Comparative benchmark script written but not yet run (needs RustFS binary)
- Streaming body support still missing (Vec<u8> buffering, OOM on large objects)

### Next Steps
1. Get RustFS binary, run comparative benchmark, record results
2. Streaming body support (critical -- listed in todo.md)
3. Split tests/s3_integration.rs (3,921 lines)
4. Failure injection tests
5. Release pipeline (dockerfile + github release)

### k3s NodePort Allocation
| Port  | Service   |
|-------|-----------|
| 30080 | Ascender  |
