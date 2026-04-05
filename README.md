# AbixIO Server

## Why AbixIO

MinIO and its closest descendants (RustFS) require identical nodes within a pool and fix parity at the erasure-set level. AbixIO drops both constraints.

**Per-object fault tolerance.** Set failures to tolerate per object. Your critical config survives 5 disk failures. Your bulk logs tolerate 1. Same bucket, same disks. One header.

**Heterogeneous everything.** Mixed disk counts, mixed node sizes, mixed volume types. One flat pool. Think vSAN meets S3.

**One node to many.** Single disk, six disks, three nodes. Same storage model. Node count changes the resilience envelope, not the product.

Early. Foundation works. Vision is what to watch.

## Status

**Early development. Not production-ready.** S3-compatible object storage server in Rust. 316 tests. 41 of 72 S3 operations implemented. No releases or production deployments yet.

If you need production S3-compatible storage today, look at [RustFS](https://github.com/rustfs/rustfs) or [SeaweedFS](https://github.com/seaweedfs/seaweedfs).

## Quick Start

```bash
cargo build --release

mkdir -p /tmp/abixio/{d1,d2,d3,d4}

./target/release/abixio \
  --volumes /tmp/abixio/d{1...4} \
  --no-auth
```

```bash
curl -X PUT http://localhost:10000/mybucket
curl -X PUT -d "hello world" http://localhost:10000/mybucket/hello.txt
curl http://localhost:10000/mybucket/hello.txt
```

Any S3 client works. 4 volumes gives you FTT=1 (tolerates 1 disk failure) by default.

## Cluster Mode

```bash
abixio --volumes /data{1...2} --nodes http://node{1...3}:10000
```

Each node gets the same `--nodes` list. Identity is resolved automatically at startup. See [cluster.md](docs/cluster.md).

## Documentation

| Doc | Subject |
|---|---|
| [architecture.md](docs/architecture.md) | Design, project structure |
| [comparison.md](docs/comparison.md) | Differentiation vs MinIO, RustFS, Garage, SeaweedFS, Ceph, Swift |
| [cluster.md](docs/cluster.md) | Cluster control, quorum, fencing |
| [storage-layout.md](docs/storage-layout.md) | On-disk metadata, volume identity |
| [per-object-ec.md](docs/per-object-ec.md) | FTT, per-object erasure coding |
| [s3-compliance.md](docs/s3-compliance.md) | S3 API coverage audit |
| [admin-api.md](docs/admin-api.md) | Admin endpoints |
| [healing.md](docs/healing.md) | Erasure healing, MRF queue, scanner |

## Related

- **[abixio-ui](https://github.com/abix-/abixio-ui)**: desktop S3 manager and AbixIO admin UI

## License

[GNU General Public License v3.0](LICENSE)
