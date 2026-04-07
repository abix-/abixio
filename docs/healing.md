# How AbixIO Healing Works

AbixIO automatically detects and repairs degraded objects. When a disk fails,
a shard gets corrupted by bitrot, or a write partially fails, AbixIO rebuilds
the missing data from the remaining shards using Reed-Solomon erasure coding.

## The Problem

With FTT=2 on 4 disks, each object is split into 2 data shards and 2 parity
shards. You can lose any 2 shards and still recover the
original data. But the lost shards need to be rebuilt, otherwise the next
failure could be fatal.

Healing is what rebuilds those shards.

## Two Healing Paths

### Path 1: MRF (Most Recently Failed), Reactive

When a PUT succeeds with quorum but fails on some disks, the object is
immediately enqueued for repair.

```
Client PUT "photos/cat.jpg"
  -> Write shard to disk 0: OK
  -> Write shard to disk 1: OK
  -> Write shard to disk 2: FAIL (disk full)
  -> Write shard to disk 3: OK
  -> Quorum met (3 >= 3), return 200 OK to client
  -> Enqueue ("mybucket", "photos/cat.jpg") to MRF queue
```

The MRF drain worker picks up the entry and calls `heal_object()`, which
reads the 3 good shards, reconstructs the missing one, and writes it to
disk 2.

**Properties:**
- Bounded queue (1000 entries)
- Deduplicated: same (bucket, key) won't be queued twice
- Best-effort: if the queue is full, the entry is dropped (the scanner
  will catch it later)
- Triggered within milliseconds of the partial failure
- Current implementation runs a single drain worker even though the CLI still
  exposes `--mrf-workers`

### Path 2: Integrity Scanner, Proactive

A background task runs every `--scan-interval` (default 10 minutes). It walks
every object on every bucket and verifies shard integrity across all disks.

```
Scanner wakes up
  -> List buckets: ["photos", "docs"]
  -> For bucket "photos":
     -> List objects: ["cat.jpg", "dog.jpg", "bird.jpg"]
     -> For "cat.jpg":
        -> Last checked 2 minutes ago, heal-interval is 24h -> skip
     -> For "dog.jpg":
        -> Never checked before
        -> Read meta.json from all 4 disks
        -> Read shard.dat from all 4 disks, verify SHA256 checksums
        -> Disk 2 shard checksum mismatch (bitrot!)
        -> Enqueue ("photos", "dog.jpg") to MRF queue
        -> Mark as checked (won't re-check for 24h)
```

**Properties:**
- Cooldown per object: won't re-check within `--heal-interval` (default 24h)
- Catches anything MRF missed (queue overflow, objects degraded by external
  causes like manual file deletion or bitrot)
- Runs as one async scan cycle per interval

## The heal_object() Function

This is the core repair logic. Called by both MRF worker and scanner.

The healer is **per-object FTT aware**: it reads each object's `erasure.ftt`
from the stored metadata rather than using global config. A volume pool can
contain objects with different FTT (e.g., one at FTT=5 and another at FTT=2),
and each is healed using its own parameters.

### Step 1: Read All Disks

Read `meta.json` and `shard.dat` from every disk for the given (bucket, key).
Some disks may return errors (missing files, IO errors). The object's EC params
(`data`, `parity`) are extracted from the first available metadata.

### Step 2: Find Consensus Metadata

Group the metadata by `quorum_eq()`. Two metadata records are
quorum-compatible if they match on all fields except the per-shard `index`
and `checksum` (which are different for every shard by design).

When placement-aware metadata is present, `quorum_eq()` also requires
`epoch_id` and `volume_ids` to match.

Pick the group with the most members. This must have at least `data_n`
members to be valid.

```
Example with 4 disks, FTT=2 (2 data + 2 parity shards):
  Disk 0: meta OK, matches consensus
  Disk 1: meta OK, matches consensus
  Disk 2: meta MISSING
  Disk 3: meta OK, matches consensus
  -> Consensus has 3 members (>= data_n=2), valid
```

### Step 3: Classify Each Shard

Read each disk and use `erasure.index` from that disk's metadata to identify
which shard it holds. Classify each shard position:

- **Good**: meta matches consensus AND `sha256(shard.dat) == meta.checksum`
- **NeedsRepair**: meta missing, doesn't match consensus, or checksum mismatch

```
Disk 0: erasure.index=1, checksum OK  -> Shard 1: Good
Disk 1: erasure.index=3, checksum OK  -> Shard 3: Good
Disk 2: MISSING                       -> Shard 0: NeedsRepair
Disk 3: erasure.index=2, checksum OK  -> Shard 2: Good
```

### Step 4: Can We Heal?

Count good shards. If `good_count < data_n`, we don't have enough data to
reconstruct. Return `Unrecoverable`.

```
good_count = 3, data_n = 2  -> 3 >= 2, can heal
```

### Step 5: Reed-Solomon Reconstruct

Pass the shard array (with `None` for missing shards) to the Reed-Solomon
`reconstruct()` function. It fills in the missing shards using the
mathematical relationship between data and parity shards.

### Step 6: Write Repaired Shards

For each shard marked `NeedsRepair`, write the reconstructed data to the
correct disk atomically:

1. Write `shard.dat` and `meta.json` to a temp directory
2. Rename temp directory to the final object directory

This is the same atomic write pattern used by normal PUTs.

## What Can and Cannot Be Healed

| Scenario | FTT=2 (2+2) | Result |
|---|---|---|
| All 4 shards healthy | No healing needed | Healthy |
| 1 shard missing | 3 good >= 2 needed | Repaired |
| 2 shards missing | 2 good >= 2 needed | Repaired |
| 3 shards missing | 1 good < 2 needed | Unrecoverable |
| 1 shard corrupted (bitrot) | Treated as missing | Repaired |
| 2 corrupt + 1 missing | 1 good < 2 needed | Unrecoverable |

**General rule:** You can lose up to `parity_n` shards and still heal.
If you lose more than `parity_n`, the object is unrecoverable.

With per-object EC, different objects in the same bucket can have different
tolerance levels. An object stored with EC 1+5 survives 5 disk failures,
while an object stored with EC 4+2 in the same bucket only survives 2.

## Configuration

```
--scan-interval 10m     # how often the scanner runs (default: 10m)
--heal-interval 24h     # per-object recheck cooldown (default: 24h)
--mrf-workers 2         # exposed in admin/status, but current runtime uses one drain worker
```

## On-Disk Layout During Healing

```
Before healing (shard missing on disk 2):
  disk0/mybucket/photo.jpg/shard.dat  [shard 1, OK]
  disk0/mybucket/photo.jpg/meta.json
  disk1/mybucket/photo.jpg/shard.dat  [shard 3, OK]
  disk1/mybucket/photo.jpg/meta.json
  disk2/mybucket/photo.jpg/            [MISSING]
  disk3/mybucket/photo.jpg/shard.dat  [shard 2, OK]
  disk3/mybucket/photo.jpg/meta.json

After healing:
  disk2/mybucket/photo.jpg/shard.dat  [shard 0, RECONSTRUCTED]
  disk2/mybucket/photo.jpg/meta.json  [written with correct index/checksum]
```

## Graceful Shutdown

When the server receives ctrl-c:
1. Signal scanner and MRF worker to stop
2. The process exits from the main loop

Current limitation:

- shutdown signalling exists for background heal tasks, but the server does not
  yet implement a fully coordinated graceful HTTP drain-and-wait shutdown path
