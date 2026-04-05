# AbixIO Server

Rust S3-compatible object storage where each object chooses its own fault tolerance. Mix any disks, any nodes, one pool. Single machine or cluster.

> **Home lab use only.** Early development. Do not store business data on this.
> For production, see [RustFS](https://github.com/rustfs/rustfs) or [SeaweedFS](https://github.com/seaweedfs/seaweedfs).

### Quick start

```bash
cargo build --release
mkdir -p /tmp/abixio/{d1,d2,d3,d4}

./target/release/abixio --volumes /tmp/abixio/d{1...4} --no-auth
```

```bash
curl -X PUT http://localhost:10000/mybucket
curl -X PUT -d "hello world" http://localhost:10000/mybucket/hello.txt
curl http://localhost:10000/mybucket/hello.txt
```

Any S3 client works. 4 volumes = FTT 1 (tolerates 1 disk failure) by default.

### Cluster

```bash
abixio --volumes /data{1...2} --nodes http://node{1...3}:10000
```

Same `--nodes` list on every node. Identity resolves automatically. See [cluster docs](docs/cluster.md).

### Current state

| | |
|---|---|
| **Tests** | 316 (unit + integration) |
| **S3 coverage** | 41 of 72 operations ([details](docs/s3-compliance.md)) |
| **Releases** | None yet |

### Docs

[Architecture](docs/architecture.md) -- [Storage layout](docs/storage-layout.md) -- [Per-object EC](docs/per-object-ec.md) -- [Cluster](docs/cluster.md) -- [Admin API](docs/admin-api.md) -- [Healing](docs/healing.md) -- [S3 compliance](docs/s3-compliance.md) -- [Comparison](docs/comparison.md)

### Related

[abixio-ui](https://github.com/abix-/abixio-ui) -- desktop S3 manager and AbixIO admin UI

### License

[GPLv3](LICENSE)
