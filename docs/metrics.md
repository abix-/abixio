# Metrics

Abixio exports Prometheus-format metrics at `/_admin/metrics`. Scrape
it from any Prometheus server; the text format is the canonical
one (`Content-Type: text/plain; version=0.0.4`).

```bash
curl -s http://127.0.0.1:10000/_admin/metrics | grep abixio_
```

The endpoint is served by the admin dispatcher, so the cluster fence
still applies when the server is in a degraded state. No auth is
required on the endpoint itself (standard Prometheus practice). Set
`--metrics-enable=false` at startup to disable the endpoint and stop
recording counters.

## Naming and labels

All metrics are prefixed `abixio_`. Labels are lowercase and follow
the op name as it appears in the s3s DTO (`GetObject`, `PutObject`,
...) where relevant.

## Families

### Request path

| Name | Type | Labels | Meaning |
|---|---|---|---|
| `abixio_s3_requests_total` | Counter | `op`, `status_class` | S3 requests served. `status_class` is one of `ok`, `client_err`, `server_err`. |
| `abixio_s3_request_duration_seconds` | Histogram | `op` | Wall-clock request duration. Buckets: 1ms, 5ms, 10ms, 50ms, 100ms, 500ms, 1s, 5s, 10s. |
| `abixio_s3_bytes_sent_total` | Counter | `op` | Response body bytes sent. |
| `abixio_s3_bytes_received_total` | Counter | `op` | Request body bytes received. |

PromQL examples:
```promql
# error rate over 5m
sum(rate(abixio_s3_requests_total{status_class="server_err"}[5m])) by (op)

# p99 latency per op over 5m
histogram_quantile(0.99, sum(rate(abixio_s3_request_duration_seconds_bucket[5m])) by (op, le))
```

### Caches

| Name | Type | Labels | Meaning |
|---|---|---|---|
| `abixio_write_cache_hits_total` | Counter | — | GET/HEAD served from the RAM write cache. |
| `abixio_write_cache_bytes` | Gauge | — | Bytes in the write cache. |
| `abixio_write_cache_entries` | Gauge | — | Entries in the write cache. |
| `abixio_write_cache_replicate_success_total` | Counter | — | Successful peer cache replications. |
| `abixio_write_cache_replicate_fail_total` | Counter | — | Peer cache replication failures (fell through to disk). |
| `abixio_read_cache_hits_total` | Counter | — | GETs served from the LRU read cache. |
| `abixio_read_cache_misses_total` | Counter | — | GETs that missed the LRU read cache and went to disk. |
| `abixio_read_cache_bytes` | Gauge | — | Bytes in the read cache. |
| `abixio_read_cache_entries` | Gauge | — | Entries in the read cache. |

```promql
# read cache hit rate over 5m
rate(abixio_read_cache_hits_total[5m]) /
  (rate(abixio_read_cache_hits_total[5m]) + rate(abixio_read_cache_misses_total[5m]))
```

### WAL

| Name | Type | Labels | Meaning |
|---|---|---|---|
| `abixio_wal_pending_entries` | Gauge | `disk` | Un-materialized entries in the WAL on each disk. |
| `abixio_wal_materialized_total` | Counter | — | WAL entries written to the file tier. |
| `abixio_wal_materialize_failed_total` | Counter | — | WAL materialize attempts that errored. |

### Storage

| Name | Type | Labels | Meaning |
|---|---|---|---|
| `abixio_disk_bytes_total` | Gauge | `disk` | Filesystem total bytes per disk. |
| `abixio_disk_bytes_used` | Gauge | `disk` | Filesystem used bytes per disk. |
| `abixio_disk_bytes_free` | Gauge | `disk` | Filesystem free bytes per disk. |

### Background workers

| Name | Type | Labels | Meaning |
|---|---|---|---|
| `abixio_heal_repaired_total` | Counter | — | Objects repaired by the heal worker. |
| `abixio_heal_unrecoverable_total` | Counter | — | Objects the heal worker could not reconstruct. |
| `abixio_mrf_queue_depth` | Gauge | — | Pending heal requests in the MRF queue. |
| `abixio_scanner_objects_checked_total` | Counter | — | Objects inspected by the integrity scanner. |
| `abixio_scanner_last_pass_seconds` | Gauge | — | Wall-clock seconds of the last scanner pass. |
| `abixio_lifecycle_deleted_total` | Counter | — | Objects deleted (or delete-markered) by lifecycle. |
| `abixio_lifecycle_deleted_versions_total` | Counter | — | Noncurrent versions deleted by lifecycle. |
| `abixio_lifecycle_delete_markers_reaped_total` | Counter | — | Expired delete markers cleaned up by lifecycle. |
| `abixio_lifecycle_multipart_aborted_total` | Counter | — | Incomplete multipart uploads aborted by lifecycle. |
| `abixio_lifecycle_last_pass_seconds` | Gauge | — | Wall-clock seconds of the last lifecycle pass. |

### Cluster

| Name | Type | Labels | Meaning |
|---|---|---|---|
| `abixio_cluster_state` | Gauge | `state` | One series per state (`Ready`, `SyncingEpoch`, `Fenced`, `Joining`), 1 for current, 0 otherwise. |
| `abixio_cluster_reachable_voters` | Gauge | — | Voters reachable from this node including self. |
| `abixio_cluster_quorum` | Gauge | — | Voters required for quorum. |
| `abixio_cluster_epoch_id` | Gauge | — | Current cluster epoch id. |

## Not (yet) covered

- OpenTelemetry traces / metrics / logs export
- Structured JSON log formatter with rotation and compression
- Audit log subsystem
- Bundled Grafana dashboards
- Host-level CPU / memory / network metrics
- Per-bucket request counters (labels omitted intentionally to keep
  cardinality bounded)
