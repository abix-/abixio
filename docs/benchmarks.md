# Benchmarks

For methodology, fairness requirements, client/server specs, and
how to run benchmarks, see
[benchmark-requirements.md](benchmark-requirements.md).

---

## L7: Full end-to-end (child process, TLS, SDK)

Measured 2026-04-13. AbixIO child process, HTTPS + SigV4 +
UNSIGNED-PAYLOAD, aws-sdk-s3 client. 1 disk, Defender-excluded tmpdir.
Run with: `abixio-ui bench --layers L7 --write-paths file,wal --write-cache both --servers abixio --clients sdk`

4 configs: file, file+wc, wal, wal+wc. Competitive comparison
(RustFS, MinIO) pending separate run.

### L7 PUT unsigned (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 2.8 | **8.6** | 3.2 | **7.8** |
| 64KB | 37.5 | **46.2** | 31.3 | 39.4 |
| 10MB | 349.1 | **400.0** | **357.0** | 352.4 |
| 1GB | **490.1** | 479.4 | 317.6 | 314.3 |

Write cache dominates at 4KB: file+wc (8.6 MB/s) is 3x raw file
(2.8) and 2.7x raw WAL (3.2). At 64KB file+wc still leads. At 10MB
the tiers converge. At 1GB file wins by 1.5x over WAL.

### L7 PUT signed (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 2.8 | **8.0** | 2.9 | **7.5** |
| 64KB | **30.4** | **32.1** | 27.8 | 27.6 |
| 10MB | **110.4** | **110.8** | 97.7 | 97.8 |
| 1GB | **112.7** | **112.5** | 92.4 | 94.4 |

Signed PUT is 3-4x slower than unsigned at 10MB+ because the SDK
must SHA-256 the entire body before sending. The tier/cache
differences are compressed under the signing overhead.

### L7 GET (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 3.4 | **5.6** | 3.0 | **5.5** |
| 64KB | 38.9 | **44.5** | **39.0** | 39.3 |
| 10MB | 278.0 | **279.3** | 275.7 | 263.1 |
| 1GB | **293.8** | 283.0 | 269.6 | 260.8 |

GET is equalized across tiers at medium+ sizes (disk read dominates).
Write cache helps GET at 4KB because recently-written objects are
served from RAM.

### L7 metadata ops (p50 latency)

| Op | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| HEAD | 501us | **450us** | 460us | 472us |
| LIST (100 obj) | **20.1ms** | 20.7ms | 21.0ms | 21.5ms |
| DELETE | **794us** | 732us | 906us | 905us |

Metadata ops are tier-independent. HEAD ~450-500us, DELETE ~730-900us,
LIST 100 objects ~20ms.

---

## L3: Storage pipeline (direct API, no HTTP)

Measured 2026-04-13. 1 disk, Defender-excluded tmpdir.
Run with: `abixio-ui bench --layers L3 --write-paths file,wal --write-cache both`

4 configs: file, file+wc, wal, wal+wc. All sizes.

### L3 PUT (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 5.3 | 5.2 | **28.6** | **28.9** |
| 64KB | 69.5 | 66.8 | **194.4** | **196.5** |
| 10MB | **422.1** | **418.5** | 379.9 | 380.0 |
| 100MB | **443.5** | **433.2** | 317.4 | 318.7 |
| 1GB | **452.2** | 436.7 | 320.1 | 320.5 |

WAL wins at 4KB (5.4x) and 64KB (2.8x). File wins at 10MB+ (1.1x
to 1.4x). The crossover is between 64KB and 10MB. This confirms
the production dispatch: WAL for <=64KB, file for >64KB.

Write cache has negligible impact on L3 PUT -- it adds a DashMap
insert but the tier's own write dominates.

### L3 GET (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | **9.1** | 8.8 | 7.5 | 7.5 |
| 64KB | **129.1** | 125.9 | 114.1 | 114.5 |
| 10MB | **1272.1** | 1165.7 | 1202.9 | 1206.8 |
| 100MB | 1251.9 | **1310.7** | 1290.7 | 1296.9 |
| 1GB | **1303.3** | 1259.5 | 1254.5 | 1230.7 |

File GET is slightly faster at small sizes (no WAL index lookup).
At large sizes both tiers converge because disk read dominates.

### L3 streaming PUT (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 5.4 | 5.0 | **27.9** | **28.0** |
| 64KB | 69.6 | 69.2 | **198.6** | **200.6** |
| 10MB | **513.3** | **515.5** | 366.5 | 367.7 |
| 100MB | **536.2** | **548.2** | 329.3 | 329.7 |
| 1GB | **533.1** | 523.5 | 316.4 | 314.6 |

Same WAL-wins-small, file-wins-large pattern. Streaming PUT is
faster than buffered PUT at large sizes for the file tier because
it avoids buffering the full body.

---

## L6: S3 + real storage (in-process, no TLS)

Measured 2026-04-13. 1 disk, Defender-excluded tmpdir. In-process
s3s + VolumePool, reqwest client over TCP loopback.
Run with: `abixio-ui bench --layers L6 --write-paths file,wal --write-cache both`

### L6 PUT (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 5.4 | **16.3** | 13.1 | **16.2** |
| 64KB | 77.2 | 91.6 | **147.5** | **149.1** |
| 10MB | **366.6** | **369.1** | 236.2 | 234.4 |
| 100MB | **813.1** | **830.2** | 412.9 | 418.7 |
| 1GB | **717.4** | **707.0** | 355.9 | 362.7 |

At 4KB through L6, the write cache is the standout: file+wc (16.3
MB/s) beats raw WAL (13.1 MB/s) because the cache short-circuits
the file tier's mkdir+write. WAL still wins at 64KB without cache.
File wins decisively at 10MB+ (2x faster than WAL at 100MB+).

### L6 GET (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 7.1 | 7.2 | 6.7 | **7.8** |
| 64KB | **90.6** | 86.8 | 98.0 | **98.0** |
| 10MB | **1093.1** | 1048.3 | 1082.2 | 1088.7 |
| 100MB | 1311.4 | **1329.5** | **1352.1** | 1347.2 |
| 1GB | **1160.3** | 1104.7 | **1249.5** | 1155.6 |

GET performance converges across tiers -- disk read dominates.

---

## Single-server detailed benchmark

AbixIO 1-disk, all operations, all sizes. Measured 2026-04-09 (pre-TLS,
HTTP-only, file tier, no write cache). Numbers differ from the TLS
matrix above because TLS + SigV4 add per-request overhead.

Run with: `abixio-ui bench --layers L6 --servers abixio --write-paths file --write-cache off`

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

See the [Comprehensive matrix](#comprehensive-matrix) above for the
canonical cross-client numbers (aws-sdk-s3, AWS CLI, rclone). The older
`curl` / `mc` table has been retired because it no longer matches the
benchmark design.

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

These were both moved out of this doc. Each storage tier (file, wal)
and each layer of the write path (hyper, s3s, AbixioS3, VolumePool,
file/wal storage work) is documented end-to-end in
[write-path.md](write-path.md):

- [write-path.md::Where the time goes](write-path.md#where-the-time-goes)
  has the canonical PUT and GET p50 + throughput tables for the file
  and wal tiers at 4KB through 100MB.
- [write-path.md::Storage branches](write-path.md#storage-branches) has
  the per-branch deep dive for each tier with `where it wins / where
  it loses`, ack semantics, and final resting place.

This section is intentionally a pointer. Internal-bench numbers and
write-path attribution are not duplicated here so the same number does
not exist in two places.

---

For the canonical end-to-end PUT path, see [write-path.md](write-path.md).
For optimization history and allocation audit, see [layer-optimization.md](layer-optimization.md).
For write-ahead log design, see [write-wal.md](write-wal.md).
For RAM write cache design, see [write-cache.md](write-cache.md).

## Accuracy Report

Audited against the codebase on 2026-04-13.

All benchmarks live in `abixio-ui/src/bench/`. Run via `abixio-ui bench`.
See [benchmark-requirements.md](benchmark-requirements.md) for the file
layout, layer map, and testing spec.

L3, L6, and L7 results measured 2026-04-13 with 2-tier (file/wal) +
write cache matrix. L7 competitive comparison (RustFS, MinIO) pending.
