# AbixIO Server

Rust S3-compatible object storage where each object chooses its own fault tolerance. Mix any OS, any disks, any nodes, one pool.

> **Experimental. Not recommended for use.**
>
> This is a personal research project in active development. The storage layer, write path, on-disk format, and API surface are all changing regularly. Expect breaking changes between commits. Expect data loss. There are no releases, no stable format, no upgrade path, and no support.
>
> Do not store anything you care about on this. Not business data, not personal data, not a home lab backup. If you need S3-compatible storage for real use, look at [RustFS](https://github.com/rustfs/rustfs), [SeaweedFS](https://github.com/seaweedfs/seaweedfs), or [MinIO](https://github.com/minio/minio).
>
> This repo is useful if you want to read the code, run the benchmarks, or follow the design notes in `docs/`. It is not useful as a place to put files.

### Quick start

Start a single node on Windows with 2 disks.

```shell
$env:ABIXIO_ACCESS_KEY = "admin"
$env:ABIXIO_SECRET_KEY = "supersecret"
mkdir C:\data1, C:\data2
abixio --volumes C:\data{1...2}
```

```shell
mc alias set abixio http://localhost:10000 admin supersecret
mc mb abixio/mybucket
mc cp hello.txt abixio/mybucket/
mc cat abixio/mybucket/hello.txt
```

Any S3 client works (aws cli, mc, rclone, etc). By default, each object tolerates 1 volume failure.

Now add a Linux node with 3 disks.

**Linux node**:

```shell
export ABIXIO_ACCESS_KEY=admin
export ABIXIO_SECRET_KEY=supersecret
mkdir -p /data3 /data4 /data5
abixio --volumes /data{3...5} --nodes http://windows:10000,http://linux:10000
```

**Windows node**. Restart it with `--nodes`:

```shell
abixio --volumes C:\data{1...2} --nodes http://windows:10000,http://linux:10000
```

Same `--nodes` on every node. Identity resolves automatically. You now have 5 volumes across 2 nodes and 2 operating systems. `{N...M}` expands sequential ranges in `--volumes` and `--nodes`. See [cluster docs](docs/cluster.md).

### Current state

| | |
|---|---|
| **Tests** | 362 passing (lib + admin + distributed + S3 integration) |
| **S3 coverage** | 41 of 72 operations ([details](docs/s3-compliance.md)) |
| **Protocol** | [s3s](https://crates.io/crates/s3s) v0.13 (SigV4, chunked auth, smithy XML) |
| **GET perf** | 1220 MB/s at 1GB (mmap, zero-copy). Zero-alloc EC decode ([benchmarks](docs/benchmarks.md)) |
| **Small objects** | 1716 PUT/s, 805 GET/s (4KB, SDK, TLS). 4.7x faster PUT than MinIO, 5.7x faster than RustFS ([benchmarks](docs/benchmarks.md)) |
| **Releases** | None yet |

### Docs

[Architecture](docs/architecture.md) | [Storage layout](docs/storage-layout.md) | [Per-object EC](docs/per-object-ec.md) | [Cluster](docs/cluster.md) | [Admin API](docs/admin-api.md) | [Healing](docs/healing.md) | [S3 compliance](docs/s3-compliance.md) | [Comparison](docs/comparison.md) | [Benchmarks](docs/benchmarks.md) | [Layer optimization](docs/layer-optimization.md) | [Log-structured storage](docs/write-log.md)

### Related

[abixio-ui](https://github.com/abix-/abixio-ui): desktop S3 manager and AbixIO admin UI

### License

[GPLv3](LICENSE)
