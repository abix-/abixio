# Benchmarks

For methodology, fairness requirements, client/server specs, and how
to run benchmarks, see
[benchmark-requirements.md](benchmark-requirements.md).

Measured 2026-04-18. Release build. 1 disk, Defender-excluded tmpdir
at `C:\code\bench-tmp`. SDK client (aws-sdk-s3), HTTPS + SigV4,
disk-backed PUT source / GET sink.

## The AbixIO stack

The stack is explicitly tuned for two object regimes:

| Regime | Path | What handles it |
|---|---|---|
| <=64KB | RAM acceleration + WAL | write cache (RAM PUT ack), read cache (RAM GETs, warm-on-write), WAL (durable append to mmap segment, materialize to file tier in background) |
| >64KB | File tier | direct mkdir + write, no RAM caching, no WAL buffering |

The write cache, read cache, and WAL all exist to make small-object
PUT/GET fast. The file tier is the permanent home for everything
and the direct path for anything above the cache/WAL thresholds.
The canonical server config is `--write-tier wal --write-cache 256
--read-cache 256` (all defaults) and the bench defaults now match.

Result file: `bench-results/standard-2026-04-18.json`. Run:
`abixio-ui bench --servers abixio --clients sdk --layers L3,L7
--sizes 4KB,64KB,10MB,1GB --ops PUT,GET`.

---

## Canonical stack: wal + wc + rc

### 4KB / 64KB (the small-object regime)

L3 direct VolumePool:

| Size | PUT p50 | PUT MB/s | GET p50 | GET MB/s |
|---|---|---|---|---|
| 4KB | **141us** | 27.0 | 521us | 7.4 |
| 64KB | **276us** | **141.5** | 520us | **118.2** |

L7 end-to-end (TLS + SigV4 + SDK):

| Size | PUT p50 | PUT MB/s | GET p50 | GET MB/s | GET hot p50 | GET hot MB/s |
|---|---|---|---|---|---|---|
| 4KB | **1.0ms** | 3.2 | 1.1ms | 3.4 | **861us** | **4.5** |
| 64KB | **1.2ms** | 43.1 | 1.6ms | 35.1 | **1.3ms** | **49.6** |

Hot GET row reflects post-warm-on-write reads served from the LRU
read cache. At the small-object scale the TLS + SigV4 floor
dominates; raw storage work is microseconds.

### 10MB / 1GB (the file-tier regime)

Once objects exceed 64KB, write cache and read cache are bypassed.
WAL also bypasses (threshold 64KB). Writes go straight to the file
tier; GETs mmap from disk.

L3 direct VolumePool:

| Size | PUT p50 | PUT MB/s | GET p50 | GET MB/s |
|---|---|---|---|---|
| 10MB | 27.1ms | **366.4** | 11.2ms | **883.2** |
| 1GB | 3.18s | 321.1 | 1.20s | 863.7 |

L7 end-to-end (unsigned PUT, SDK GET):

| Size | PUT p50 | PUT MB/s | GET p50 | GET MB/s |
|---|---|---|---|---|
| 10MB | 27.0ms | 344.2 | 42.2ms | 221.4 |
| 1GB | 3.26s | 315.2 | 4.51s | 233.6 |

L7 signed PUT (SDK SHA-256s the body before send):

| Size | p50 | MB/s |
|---|---|---|
| 10MB | 102.6ms | 97.2 |
| 1GB | 10.61s | 96.6 |

---

## Competitive (canonical stack vs RustFS vs MinIO)

Measured 2026-04-18 on the canonical stack (AbixIO = wal+wc+rc) with
the streaming WAL-shard writer (promote-to-file at 1 MB buffered).
SDK client, HTTPS + SigV4. Results in
`bench-results/competitive-2026-04-18-streaming-v2.json`.

### Small-object PUT/GET (MB/s)

| Op | AbixIO | RustFS | MinIO | AbixIO vs best competitor |
|---|---|---|---|---|
| 4KB PUT unsigned | **2.4** | 1.4 | 1.9 | **1.26x MinIO** |
| 4KB PUT signed | **2.0** | 1.5 | 1.9 | 1.05x MinIO |
| 4KB GET cold | 3.6 | 2.4 | 3.4 | 1.06x MinIO |
| 4KB GET hot | 3.8 | 2.6 | **3.8** | tie MinIO |
| 64KB PUT unsigned | **27.5** | 19.8 | 27.4 | 1.00x MinIO |
| 64KB PUT signed | 20.8 | 14.3 | **23.4** | 0.89x MinIO |
| 64KB GET cold | 44.2 | 26.2 | **44.2** | tie MinIO |
| 64KB GET hot | **47.9** | 26.7 | 45.8 | 1.05x MinIO |

AbixIO wins or ties every small-object PUT/GET except 64KB signed
(the MD5 bottleneck on signed writes is a known SDK floor). Warm-on-write
read cache keeps 4KB/64KB GET within a cache-hit, so MinIO's mmap
scan advantage shrinks.

### Large-object PUT/GET (MB/s)

| Op | AbixIO | RustFS | MinIO |
|---|---|---|---|
| 10MB PUT unsigned | 282.0 | **418.7** | 270.6 |
| 10MB PUT signed | 104.5 | 75.8 | **115.0** |
| 10MB GET | **250.6** | 203.8 | 210.3 |
| 1GB PUT unsigned | 356.2 | **608.6** | 462.7 |
| 1GB PUT signed | 104.5 | 78.4 | **133.9** |
| 1GB GET | 250.2 | **255.6** | 240.6 |

Large unsigned PUT: AbixIO closed the gap vs RustFS from 1.87x to
1.71x at 1GB after the streaming WAL fix (prior: 317 MB/s; now
356). The remaining gap is the per-object EC pipeline cost (RS
encode + per-shard blake3 + separate shard.dat + meta.json), which
AbixIO pays even on 1 disk / ftt=0. Known gap -- see
[write-path.md::Design gap: unnecessary EC on single disk].

Large signed PUT: AbixIO beats RustFS at 10MB and 1GB (the signed
floor is MD5 at ~100 MB/s across all three; implementation overhead
determines the rest).

Large GET: AbixIO wins 10MB, ties 1GB.

### Metadata (p50 latency)

| Op | AbixIO | RustFS | MinIO |
|---|---|---|---|
| HEAD | 600us | 677us | **510us** |
| LIST (100 obj) | 21.2ms | 29.8ms | **6.8ms** |
| DELETE | **1.0ms** | 2.2ms | 1.3ms |

AbixIO wins DELETE and sits between RustFS and MinIO on HEAD.
MinIO's 3x lead on LIST is the remaining metadata gap -- its
single-file metadata layout is a cheap scan for bucket listings
where AbixIO walks a directory tree.

---

## Ablation: what each layer contributes

Run the ablation matrix with
`abixio-ui bench --write-paths file,wal --write-cache both
--read-cache both`. Results in
`bench-results/rc-axis-2026-04-18.json`.

### Read cache delta at 4KB L7 GET p50

| Config | rc off | rc on | Speedup |
|---|---|---|---|
| file | 1.3ms | 875us | 1.49x |
| file+wc | 1.3ms | 891us | 1.46x |
| wal | 1.4ms | 904us | 1.55x |
| wal+wc | 1.5ms | **807us** | **1.86x** |

### Read cache delta at 64KB L7 GET p50

| Config | rc off | rc on | Speedup |
|---|---|---|---|
| file | 1.8ms | 1.2ms | 1.50x |
| file+wc | 1.7ms | 1.2ms | 1.42x |
| wal | 1.8ms | 1.2ms | 1.50x |
| wal+wc | 1.8ms | 1.2ms | 1.50x |

### Write cache delta at 4KB L7 PUT p50 (unsigned)

| Config | wc off | wc on | Speedup |
|---|---|---|---|
| file | 1.4ms | 824us | 1.70x |
| wal | 1.2ms | 1.1ms | 1.09x |

Write cache helps most when the disk tier is slow (file tier
mkdir+create). WAL already has a fast-ack path so the cache layered
on top adds marginal value -- its main purpose in the wal+wc+rc
stack is primary durability before the WAL's own ack (see
[write-cache.md](write-cache.md)).

### WAL delta at 4KB L3 PUT p50

| Tier | p50 | Speedup over file |
|---|---|---|
| file | 729us | 1.0x |
| wal | **136us** | **5.4x** |

The WAL appends a checksummed needle directly into an mmap segment
(zero syscalls, zero allocation). At 4KB this is 5.4x the file
tier; at 64KB it is 3.1x. Above 64KB the WAL is not in the path.

---

## Layer floors (L1-L5) on the canonical stack

These do not change with config; they set the physical upper
bounds the small-object and file-tier regimes are competing
against. See `bench-results/main-2026-04-18.json`.

| Size | L1 http_put | L2 s3s_put | L5 disk_write | L5 disk_write_fsync |
|---|---|---|---|---|
| 4KB | 41.0 | 65.0 | 14.6 | 1.5 |
| 64KB | 506.8 | 617.9 | 208.8 | 23.2 |
| 10MB | 514.5 | 1637.9 | 2007.5 | 629.0 |
| 1GB | 753.2 | 1754.3 | 1005.5 | 1109 |

All MB/s. The hyper + s3s floors are comfortably above even the
1GB write tier. MD5 at 705 MB/s is the bottleneck for signed PUT
above 10MB; blake3 (4273 MB/s) and RS encode (2813 MB/s) are
invisible in the pipeline.

---

## Where to look next

- End-to-end PUT path with tier attribution: [write-path.md](write-path.md)
- WAL internals + materialize worker: [write-wal.md](write-wal.md)
- RAM write cache design + peer replication:
  [write-cache.md](write-cache.md)
- Architecture overview: [architecture.md](architecture.md)
- Optimization history (not a reference for current state):
  [layer-optimization.md](layer-optimization.md)

## Accuracy Report

Audited against the codebase on 2026-04-18.

- Canonical wal+wc+rc numbers above measured 2026-04-18,
  single-config run, 1000 iters per op, release build
  (`bench-results/standard-2026-04-18.json`).
- Ablation matrix (8 configs) measured 2026-04-18 at 4KB/64KB
  (`bench-results/rc-axis-2026-04-18.json`).
- Competitive (RustFS/MinIO) measured 2026-04-18 on the canonical
  stack with the streaming WAL-shard writer
  (`bench-results/competitive-2026-04-18-streaming-v2.json`). The
  pre-streaming canonical run is archived at
  `bench-results/competitive-2026-04-18-canonical.json` and the
  older 8-config run at `bench-results/competitive-2026-04-18.json`
  for ablation reference.
- All benchmarks live in `abixio-ui/src/bench/`. Run via
  `abixio-ui bench`. Defaults now match the production server
  stack (wal + wc + rc). Pass `--write-paths file,wal --write-cache
  both --read-cache both` to run the ablation matrix.
