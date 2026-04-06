# AbixIO Server

Rust S3-compatible object storage where each object chooses its own fault tolerance. Mix any OS, any disks, any nodes, one pool.

> **Home lab use only.** Early development. Do not store business data on this.
> For production, see [RustFS](https://github.com/rustfs/rustfs) or [SeaweedFS](https://github.com/seaweedfs/seaweedfs).

### Quick start

Start a single node on Windows with one disk.

```powershell
mkdir C:\abixio\d1
abixio --volumes C:\abixio\d1 --no-auth
```

```powershell
curl -X PUT http://localhost:10000/mybucket
curl -X PUT -d "hello world" http://localhost:10000/mybucket/hello.txt
curl http://localhost:10000/mybucket/hello.txt
```

Any S3 client works. One node and one disk means no redundancy, but everything works.

Now add a Linux node with 2 disks. Nodes do not need to match.

**Linux node** (192.168.1.20):

```bash
mkdir -p /srv/abixio/{d1,d2}
abixio --volumes /srv/abixio/d{1...2} \
  --nodes http://192.168.1.10:10000,http://192.168.1.20:10000 \
  --no-auth
```

**Windows node** (192.168.1.10). Restart it with `--nodes`:

```powershell
abixio --volumes C:\abixio\d1 --nodes http://192.168.1.10:10000,http://192.168.1.20:10000 --no-auth
```

Same `--nodes` on every node. Identity resolves automatically. You now have 3 volumes across 2 nodes, 2 operating systems, and different disk counts. FTT 1 tolerates 1 disk failure. See [cluster docs](docs/cluster.md).

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
