# Benchmarks

For methodology, fairness requirements, client/server specs, and
how to run benchmarks, see
[benchmark-requirements.md](benchmark-requirements.md).

Measured 2026-04-18 on the WAL + write-cache + read-cache +
kovarex-enforcement code (post 0a8434c). Release build. 1 disk,
Defender-excluded tmpdir at `C:\code\bench-tmp`. SDK client
(aws-sdk-s3), HTTPS + SigV4, disk-backed PUT source / GET sink.

- L1-L7 abixio-only: `bench-results/main-2026-04-18.json`
  (run: `abixio-ui bench --servers abixio --clients sdk
  --write-paths file,wal --write-cache both`)
- L7 competitive vs RustFS + MinIO:
  `bench-results/competitive-2026-04-18.json` (run:
  `abixio-ui bench --servers abixio,rustfs,minio --clients sdk
  --layers L7 --sizes 4KB,64KB,10MB,1GB --write-paths file,wal
  --write-cache both`)

---

## L7: Full end-to-end (child process, TLS, SDK)

AbixIO child process, HTTPS + SigV4, aws-sdk-s3 client, disk-backed
PUT source / GET sink. 4 configs: file, file+wc, wal, wal+wc.

### L7 PUT unsigned (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 2.6 | **3.0** | 2.8 | 2.8 |
| 64KB | 35.9 | **39.5** | 28.7 | 31.8 |
| 10MB | 286.4 | 241.1 | **317.3** | 309.5 |
| 100MB | 241.7 | 250.3 | **286.0** | 285.1 |
| 1GB | 273.1 | 273.4 | 294.2 | **306.7** |

WAL leads at 10MB+ (dispatch threshold is 64KB, so any object above
that flows through the file tier at L3 but clients here hit the
full stack including s3s + auth). Write cache adds modest headroom
at small sizes only.

### L7 PUT signed (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 2.5 | **3.0** | 2.8 | 2.8 |
| 64KB | 28.3 | **35.3** | 26.2 | 26.9 |
| 10MB | 108.9 | **109.5** | 92.5 | 94.4 |
| 100MB | 109.0 | 109.0 | 93.5 | 93.4 |
| 1GB | **107.3** | 105.5 | 91.9 | 94.4 |

Signed PUT is 2.5-3x slower than unsigned at 10MB+ because the SDK
SHA-256s the full body before sending. File tier wins here because
the WAL dispatch threshold does not save work once the signing
bottleneck dominates.

### L7 GET (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | **3.0** | 2.9 | 2.8 | 2.7 |
| 64KB | 23.1 | 22.9 | 35.9 | **36.8** |
| 10MB | **264.1** | 263.1 | 245.5 | 243.7 |
| 100MB | **286.9** | 268.3 | 251.8 | 249.7 |
| 1GB | **265.3** | 268.8 | 258.0 | 256.1 |

GET is tier-independent at medium+ sizes (disk read dominates).
WAL 64KB GET is unexpectedly faster than file; this is a warm-log
read effect (recent entries stay in the mmap region).

### L7 metadata ops (p50 latency)

| Op | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| HEAD | 692us | 595us | 598us | **566us** |
| LIST (100 obj) | 20.8ms | 21.0ms | **20.4ms** | 21.0ms |
| DELETE | **953us** | 1.0ms | 1.0ms | 1.1ms |

Metadata is tier-independent. HEAD 566-692us, DELETE ~950us-1.1ms,
LIST 100 objects ~20-21ms. Write cache trims HEAD by ~100us (serves
from DashMap).

---

## L7: Read cache for 4KB/64KB GET (warm-on-write)

Measured 2026-04-18, 1000 iters per op, SDK client. The read cache
is populated on PUT for objects <= 64KB (warm-on-write) and on GET
for anything that still misses. Subsequent GETs are served from RAM
without touching disk.

Run: `abixio-ui bench --layers L7 --sizes 4KB,64KB --servers abixio
--clients sdk --write-paths file,wal --write-cache both
--read-cache both --ops PUT,GET`. Results in
`bench-results/rc-axis-2026-04-18.json`.

### L7 4KB GET with / without read cache

| Config | p50 (rc off) | p50 (rc on) | Speedup |
|---|---|---|---|
| file | 1.3ms | **875us** | 1.49x |
| file+wc | 1.3ms | **891us** | 1.46x |
| wal | 1.4ms | **904us** | 1.55x |
| wal+wc | 1.5ms | **807us** | **1.86x** |

`get_hot` (re-GETs over a fixed 50-key working set, 20 rounds) sits
within 5% of the regular GET on rc-on configs, confirming the
normal bench access pattern already saturates the cache thanks to
warm-on-write -- there is no "first-read penalty" to hide.

### L7 64KB GET with / without read cache

| Config | p50 (rc off) | p50 (rc on) | Speedup |
|---|---|---|---|
| file | 1.8ms | **1.2ms** | 1.50x |
| file+wc | 1.7ms | **1.2ms** | 1.42x |
| wal | 1.8ms | **1.2ms** | 1.50x |
| wal+wc | 1.8ms | **1.2ms** | 1.50x |

### L7 PUT is unchanged by the read cache

Warm-on-write adds a single ~60KB memcpy worst case. That sits
inside the existing PUT noise floor, so the put / put_signed rows
are within run-to-run variance across rc on/off.

### What this means

- The `wal+wc+rc` stack is the canonical small-object setup: RAM
  caches in both directions, WAL for durable-ack fast writes, file
  tier behind it as the permanent home.
- A GET of a just-written <= 64KB object is a RAM path with a
  single SDK round-trip. p50 ~800us at 4KB is almost entirely TLS
  + SigV4 + hyper -- the storage itself contributes microseconds.
- Above 64KB, the read cache does not apply (ceiling). GET goes
  straight to mmap from file tier; see the standard L7 GET table.

---

## L7: Competitive (AbixIO vs RustFS vs MinIO)

Same harness, same TLS, same SDK, same warmup. Numbers are from the
competitive run and differ slightly from the abixio-only tables above
because of per-run variance. Best abixio config per size is shown.

### L7 4KB-64KB (MB/s, SDK, HTTPS)

| Op | AbixIO best | RustFS | MinIO |
|---|---|---|---|
| 4KB PUT unsigned | **3.0** (file+wc) | 1.5 | 1.9 |
| 4KB PUT signed | **2.9** (file+wc) | 1.4 | 1.9 |
| 4KB GET | 3.0 (file+wc) | 2.4 | **3.6** |
| 64KB PUT unsigned | **40.8** (file+wc) | 20.9 | 26.5 |
| 64KB PUT signed | **33.7** (file+wc) | 16.1 | 23.0 |
| 64KB GET | 35.2 (wal+wc) | 27.7 | **40.8** |

AbixIO's write cache dominates small-object PUT: 2.0x faster than
RustFS and 1.5x faster than MinIO at 64KB. MinIO wins small GET
because it has no per-object shard file or meta.json to mmap
(single-file inlined meta).

### L7 10MB-1GB (MB/s, SDK, HTTPS)

| Op | AbixIO best | RustFS | MinIO |
|---|---|---|---|
| 10MB PUT unsigned | 293.6 (wal+wc) | **432.8** | 269.7 |
| 10MB PUT signed | **108.1** (file) | 75.1 | 113.8 |
| 10MB GET | **262.4** (file+wc) | 215.0 | 225.2 |
| 1GB PUT unsigned | 326.8 (wal+wc) | **540.3** | 448.5 |
| 1GB PUT signed | 105.7 (file+wc) | 79.0 | **130.4** |
| 1GB GET | 261.0 (wal+wc) | **297.8** | 252.8 |

RustFS wins large unsigned PUT by a wide margin. AbixIO pays the
per-object EC overhead (RS encode, per-shard blake3, separate
shard.dat + meta.json) on every write, even at ftt=0 on 1 disk.
See [write-path.md::Design gap: unnecessary EC on single disk] for
the known fix path.

### L7 metadata (p50 latency)

| Op | AbixIO best | RustFS | MinIO |
|---|---|---|---|
| HEAD | 602us (wal) | 577us | **532us** |
| LIST (100 obj) | 20.1ms (wal) | 31.3ms | **6.8ms** |
| DELETE | **944us** (file+wc) | 2.1ms | 1.3ms |

MinIO's LIST is 3x faster than AbixIO; its single-file metadata
layout is a serious win for bucket scans. AbixIO wins DELETE by a
comfortable margin against both competitors.

### Summary

- **Small PUT**: AbixIO wins unsigned and signed at 4KB-64KB thanks
  to the write cache and WAL.
- **Large PUT**: RustFS wins unsigned; MinIO wins signed 1GB.
  AbixIO's per-object EC pipeline is the bottleneck.
- **Small GET**: MinIO wins; AbixIO is within 20%.
- **Large GET**: AbixIO and RustFS swap the lead by size; MinIO
  trails slightly.
- **LIST**: MinIO 3x ahead. Our listing needs work.
- **DELETE**: AbixIO wins clearly.

---

## L3: Storage pipeline (direct API, no HTTP)

`VolumePool` put/get, no s3s, no hyper.

### L3 PUT (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 4.9 | 5.1 | **28.3** | 28.2 |
| 64KB | 68.1 | 69.7 | 185.1 | **189.1** |
| 10MB | **416.1** | 417.7 | 368.4 | 368.6 |
| 100MB | **429.4** | 428.3 | 306.6 | 304.1 |
| 1GB | **433.7** | 432.9 | 289.5 | 309.0 |

WAL wins at 4KB (5.7x) and 64KB (2.7x). File wins at 10MB+
(1.1-1.5x). Crossover sits between 64KB and 10MB, which is why
the production dispatch threshold is 64KB. Write cache has
negligible L3 effect -- it adds a DashMap insert but the tier write
dominates.

### L3 GET (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 8.2 | **8.9** | 8.0 | 7.9 |
| 64KB | 130.8 | **136.4** | 121.9 | 113.5 |
| 10MB | 1082.8 | **1229.9** | 1140.0 | 1147.9 |
| 100MB | 1148.9 | **1152.1** | 1149.2 | 1146.9 |
| 1GB | 1191.9 | **1215.0** | 1198.7 | 1160.7 |

File GET is slightly faster at small sizes (no WAL index lookup).
At 10MB+ all tiers converge near 1.1-1.2 GB/s (mmap read ceiling).

### L3 streaming PUT (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 4.9 | 5.2 | **27.6** | 27.1 |
| 64KB | 70.9 | 71.0 | **195.9** | 192.9 |
| 10MB | 520.6 | **519.5** | 349.3 | 346.0 |
| 100MB | 545.7 | **548.3** | 293.0 | 297.0 |
| 1GB | **532.3** | 526.3 | 292.3 | 311.6 |

Streaming PUT is faster than buffered PUT at large sizes for the
file tier (avoids full-body buffering). WAL streaming matches
non-streaming because the tier always appends incrementally.

---

## L6: S3 + real storage (in-process, no TLS)

In-process s3s + `VolumePool`, reqwest over TCP loopback, no TLS.

### L6 PUT (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | 5.0 | 15.8 | 12.2 | **16.4** |
| 64KB | 69.2 | 82.2 | 133.8 | **144.8** |
| 10MB | 365.6 | **367.8** | 232.4 | 235.1 |
| 100MB | 704.9 | **710.1** | 362.1 | 355.2 |
| 1GB | 669.7 | **681.9** | 331.2 | 327.3 |

Write cache shines at 4KB through L6: file+wc (15.8 MB/s) is 3.2x
raw file and ~match to raw WAL. Above 10MB file tier wins by ~2x
(raw WAL buffer turnover caps it at 330-360 MB/s).

### L6 GET (MB/s)

| Size | file | file+wc | wal | wal+wc |
|---|---|---|---|---|
| 4KB | **7.1** | 7.0 | 6.2 | 7.1 |
| 64KB | 89.5 | 91.1 | 91.7 | **96.6** |
| 10MB | **1069.2** | 1062.7 | 1084.6 | 1049.1 |
| 100MB | 1263.9 | **1265.6** | 1243.5 | 1225.6 |
| 1GB | 1035.4 | 1027.2 | 1059.4 | **1083.3** |

GET converges across tiers at medium+ sizes (mmap dominates).

---

## L1, L2, L4, L5 floors

The transport, protocol, compute, and disk floors set the physical
upper bounds the storage tiers are competing against.

| Size | L1 http_put | L1 http_get | L2 s3s_put | L2 s3s_get | L5 disk_write | L5 disk_read |
|---|---|---|---|---|---|---|
| 4KB | 41.0 | 44.8 | 65.0 | 74.4 | 14.6 | 61.3 |
| 64KB | 506.8 | 574.9 | 617.9 | 1177.1 | 208.8 | 854.1 |
| 10MB | 514.5 | 636.4 | 1637.9 | 185229 | 2007.5 | 1281.2 |
| 100MB | 710.6 | 720.9 | 1766.5 | 1595569 | 942.6 | 1336.4 |
| 1GB | 753.2 | 725.6 | 1754.3 | 8592045 | 1005.5 | 1217.2 |

(All MB/s.) L2 `s3s_get` uses a zero-copy NullBackend so the numbers
are unrepresentative -- they just confirm the protocol adds no
measurable overhead.

| Op | 4KB | 64KB | 10MB | 1GB |
|---|---|---|---|---|
| L4 blake3 | 2478 | 4621 | 4446 | 4273 |
| L4 md5 | 667 | 698 | 705 | 705 |
| L4 sha256 | 267 | 272 | 277 | 277 |
| L4 rs_encode_3+1 | 2935 | 3107 | 2797 | 2813 |
| L5 disk_write_fsync | 1.5 | 23.2 | 629 | 1109 |

(All MB/s.) blake3 is 9x sha256 and comfortably outstrips disk, so
checksum is never the bottleneck. RS encoding at ~2.8-3.0 GB/s
through the SIMD `reed-solomon-erasure` crate is well above disk
even at large sizes.

---

## Server-side processing (historical)

Debug header `x-debug-s3s-ms` shows actual server processing time
on small objects (excludes client overhead, TCP, HTTP parsing):

```
4KB PUT server processing:  0.28ms
4KB GET server processing:  0.08ms
4KB HEAD server processing: 0.08ms
```

The server processes 4KB GET in 80us. The remaining latency in
end-to-end benchmarks is client overhead (SigV4 signing, HTTP,
connection management).

---

## Internal per-layer benchmarks

Tests each layer of the storage stack independently (no HTTP client).
Run via `abixio-ui bench --layers L1,L2,L3,L4,L5,L6` for a full
floor-to-ceiling view. See
[layer-optimization.md](layer-optimization.md) for the optimization
history that got us here.

---

## Write tier comparison and stack breakdown

Each storage tier (file, wal) and each layer of the write path
(hyper, s3s, AbixioS3, VolumePool, file/wal storage work) is
documented end-to-end in [write-path.md](write-path.md):

- [write-path.md::Where the time goes](write-path.md#where-the-time-goes)
  has the canonical PUT and GET p50 + throughput tables for the file
  and WAL tiers.
- [write-path.md::Storage branches](write-path.md#storage-branches)
  has the per-branch deep dive for each tier with where it wins /
  where it loses, ack semantics, and final resting place.

This section is intentionally a pointer. Internal-bench numbers and
write-path attribution are not duplicated here so the same number
does not exist in two places.

---

For the canonical end-to-end PUT path, see
[write-path.md](write-path.md).
For optimization history and allocation audit, see
[layer-optimization.md](layer-optimization.md).
For write-ahead log design, see [write-wal.md](write-wal.md).
For RAM write cache design, see [write-cache.md](write-cache.md).

## Accuracy Report

Audited against the codebase on 2026-04-18.

All benchmarks live in `abixio-ui/src/bench/`. Run via
`abixio-ui bench`. See
[benchmark-requirements.md](benchmark-requirements.md) for the file
layout, layer map, and testing spec.

L1-L7 results above measured 2026-04-18 with the 2-tier (file/wal)
x write cache matrix, SDK client only, release build. L7
competitive comparison against RustFS and MinIO also measured
2026-04-18 (`bench-results/competitive-2026-04-18.json`).
