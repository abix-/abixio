# Layer optimization

Per-layer performance analysis. Each layer treated as a separate problem.
Updated after each optimization with new numbers.

## Layer architecture

```
PUT path (top to bottom):
L6  S3 protocol      s3s parses HTTP, dispatches to AbixioS3 impl
L5  HTTP transport    hyper accepts TCP, reads/writes body
L4  Storage pipeline  VolumePool: placement + encode + write shards
L3  Disk I/O          tokio::fs::write / tokio::fs::read
L2  RS encode         reed-solomon-erasure galois_8, SIMD-accel
L1  Hashing           blake3 (shard integrity) + MD5 (S3 ETag)

GET path reverses: L6 -> L5 -> L4 (read shards, RS decode, reassemble) -> L3 -> L1
```

## Current numbers

From `bench_perf` at commit `dcf5e7b`, Windows 10, single machine, NTFS tmpdir.

```
Layer  What                                     4KB          10MB         1GB
-----  ---------------------------------------- -----------  -----------  -----------
L1     blake3                                    1604 MB/s    4303 MB/s    4286 MB/s
L1     MD5                                        449 MB/s     703 MB/s     700 MB/s
L2     RS encode 3+1                             2955 MB/s    2762 MB/s    2825 MB/s
L3     disk write (page cache)                     13 MB/s    1625 MB/s    1056 MB/s
L3     disk read (cached)                          46 MB/s    2703 MB/s    2902 MB/s
L4     put_stream (1 disk)                          4 MB/s     439 MB/s     493 MB/s
L4     get (1 disk)                                 8 MB/s    1178 MB/s    1256 MB/s
L4     put_stream (4 disk, 3+1 EC)                  2 MB/s     367 MB/s     416 MB/s
L4     get (4 disk, 3+1 EC)                         1 MB/s     902 MB/s     917 MB/s
L5     HTTP PUT                                    32 MB/s     762 MB/s     800 MB/s
L6     s3s PUT (1 disk)                             3 MB/s     230 MB/s     305 MB/s
L6     s3s GET (1 disk)                             6 MB/s     365 MB/s     452 MB/s
```

Competitive benchmark (sequential `mc` client, 10MB):
- AbixIO: 90 MB/s PUT, 96 MB/s GET
- RustFS: 74 MB/s PUT, 122 MB/s GET
- MinIO: 82 MB/s PUT, 108 MB/s GET

At 1GB: AbixIO 375 MB/s PUT vs RustFS 541 vs MinIO 576.

---

## L1: Hashing -- no change needed

blake3 at 4.3 GB/s (SIMD), MD5 at 703 MB/s (near theoretical max).
Both computed inline during streaming -- overlaps with I/O at large sizes.
MD5 required for S3 ETag compliance. Not a bottleneck at any layer above.

## L2: RS encode -- no change needed

2.8 GB/s with `simd-accel`. 6x faster than L4 storage pipeline.
Not a bottleneck.

## L3: Disk I/O -- no change needed

Page-cache writes: 1.6 GB/s (10MB). This is the ceiling for L4.
Future: configurable fsync for durability, io_uring on Linux.

## L4: Storage pipeline

### PUT bottleneck: sequential shard writes

`write_block()` in `src/storage/erasure_encode.rs:272`:
```rust
for (i, shard_data) in shards.iter().enumerate() {
    shard_hashers[i].update(shard_data);
    writers[i].write_chunk(shard_data).await?;  // sequential
}
```

Each write awaits before the next. With 4 disks, serializes 4 disk writes.
Previous FuturesUnordered attempt regressed at 10MB (147->74 MB/s) due to
per-block spawn overhead.

**Solution**: channel-based shard writer pipeline. Spawn one task per shard
writer at encode start. Send blocks via mpsc channels. No per-block spawn cost.

### GET bottleneck: full-body collection

`read_and_decode()` in `src/storage/erasure_decode.rs:8` reads all shards
in parallel (good) but assembles into `Vec<u8>`. For 1GB objects, allocates
1GB before response can start.

**Solution**: streaming decode. `read_and_decode_stream()` returns
`Stream<Item=Result<Bytes>>`. Read shard metadata first, then stream shard
data in 1MB blocks, RS-decode per block, yield.

## L5: HTTP transport -- no change needed

762 MB/s PUT with hyper 1.x. Not the bottleneck.

## L6: S3 protocol (s3s)

### PUT bottleneck: per-request disk reads

`put_object()` in `src/s3_service.rs:198` calls `get_versioning_config()`
on every PUT, which reads bucket settings from disk.

**Solution**: in-memory versioning config cache per bucket, invalidated on
`set_versioning_config()`.

### GET bottleneck: full-body buffering

`get_object()` in `src/s3_service.rs:261` calls `Store::get_object()` which
returns `(Vec<u8>, ObjectInfo)`. Entire object in memory before first byte
sent. Then wraps as single-chunk `StreamingBlob`.

**Solution**: wire L4 streaming decode through to s3s `StreamingBlob::wrap()`.
Requires L4 streaming decode (above) first.

---

## Implementation priority

| # | Change | Layer | Measured gain | Status |
|---|--------|-------|---------------|--------|
| 1 | Streaming GET (decode + response) | L4+L6 | L4: +9-16%. L6: **+105% (10MB), +84% (1GB)**. GET 365->750, 452->833 MB/s | done |
| 2 | Channel-based parallel shard writes | L4 | Medium (1GB PUT toward L3 ceiling) | planned |
| 3 | Versioning config cache | L6 | Small-medium (remove disk read per PUT) | planned |
