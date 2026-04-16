//! Prometheus metrics registry and handles.
//!
//! Exposed via `/_admin/metrics` in the Prometheus text format.
//! Operators scrape it with a prometheus server, graph in Grafana,
//! alert on latency tails and error rates.

use std::sync::Arc;
use std::time::Duration;

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec,
    Opts, Registry, TextEncoder,
};

pub mod http;

/// Coarse S3 operation class used as a label on request metrics.
/// Fine-grained labels (exact op name) are attached as `op`.
#[derive(Clone, Copy, Debug)]
pub enum StatusClass {
    Ok,
    ClientErr,
    ServerErr,
}

impl StatusClass {
    pub fn as_str(self) -> &'static str {
        match self {
            StatusClass::Ok => "ok",
            StatusClass::ClientErr => "client_err",
            StatusClass::ServerErr => "server_err",
        }
    }

    pub fn from_http_status(code: u16) -> StatusClass {
        match code {
            0..=399 => StatusClass::Ok,
            400..=499 => StatusClass::ClientErr,
            _ => StatusClass::ServerErr,
        }
    }
}

/// All metric handles. Cloneable `Arc` so every subsystem can grab
/// the slots it writes to without threading individual counters
/// everywhere.
pub struct Metrics {
    pub registry: Registry,

    // -- request path --
    pub s3_requests: IntCounterVec,
    pub s3_request_duration: HistogramVec,
    pub s3_bytes_sent: IntCounterVec,
    pub s3_bytes_received: IntCounterVec,

    // -- caches --
    pub write_cache_hits: IntCounter,
    pub write_cache_replicate_success: IntCounter,
    pub write_cache_replicate_fail: IntCounter,
    pub read_cache_hits: IntCounter,
    pub read_cache_misses: IntCounter,
    pub write_cache_bytes: IntGauge,
    pub write_cache_entries: IntGauge,
    pub read_cache_bytes: IntGauge,
    pub read_cache_entries: IntGauge,

    // -- WAL --
    pub wal_materialized: IntCounter,
    pub wal_materialize_failed: IntCounter,
    pub wal_pending: IntGaugeVec,

    // -- background workers --
    pub heal_repaired: IntCounter,
    pub heal_unrecoverable: IntCounter,
    pub mrf_queue_depth: IntGauge,
    pub scanner_objects_checked: IntCounter,
    pub scanner_last_pass_seconds: IntGauge,
    pub lifecycle_deleted: IntCounter,
    pub lifecycle_deleted_versions: IntCounter,
    pub lifecycle_delete_markers_reaped: IntCounter,
    pub lifecycle_multipart_aborted: IntCounter,
    pub lifecycle_last_pass_seconds: IntGauge,

    // -- storage --
    pub disk_total_bytes: IntGaugeVec,
    pub disk_used_bytes: IntGaugeVec,
    pub disk_free_bytes: IntGaugeVec,

    // -- cluster --
    pub cluster_state: IntGaugeVec,
    pub cluster_reachable_voters: IntGauge,
    pub cluster_quorum: IntGauge,
    pub cluster_epoch_id: IntGauge,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();

        let s3_requests = IntCounterVec::new(
            Opts::new("abixio_s3_requests_total", "S3 requests served, labelled by op and status class."),
            &["op", "status_class"],
        )
        .expect("register s3_requests");
        registry.register(Box::new(s3_requests.clone())).ok();

        let s3_request_duration = HistogramVec::new(
            HistogramOpts::new(
                "abixio_s3_request_duration_seconds",
                "S3 request wall-clock duration, seconds.",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0,
            ]),
            &["op"],
        )
        .expect("register s3_request_duration");
        registry.register(Box::new(s3_request_duration.clone())).ok();

        let s3_bytes_sent = IntCounterVec::new(
            Opts::new("abixio_s3_bytes_sent_total", "Response body bytes sent."),
            &["op"],
        )
        .expect("register s3_bytes_sent");
        registry.register(Box::new(s3_bytes_sent.clone())).ok();

        let s3_bytes_received = IntCounterVec::new(
            Opts::new(
                "abixio_s3_bytes_received_total",
                "Request body bytes received.",
            ),
            &["op"],
        )
        .expect("register s3_bytes_received");
        registry.register(Box::new(s3_bytes_received.clone())).ok();

        let write_cache_hits = IntCounter::new(
            "abixio_write_cache_hits_total",
            "Reads served from the RAM write cache.",
        )
        .expect("register write_cache_hits");
        registry.register(Box::new(write_cache_hits.clone())).ok();

        let write_cache_replicate_success = IntCounter::new(
            "abixio_write_cache_replicate_success_total",
            "Successful peer cache replications.",
        )
        .expect("register replicate_success");
        registry
            .register(Box::new(write_cache_replicate_success.clone()))
            .ok();

        let write_cache_replicate_fail = IntCounter::new(
            "abixio_write_cache_replicate_fail_total",
            "Peer cache replication failures that fell through to disk.",
        )
        .expect("register replicate_fail");
        registry
            .register(Box::new(write_cache_replicate_fail.clone()))
            .ok();

        let read_cache_hits = IntCounter::new(
            "abixio_read_cache_hits_total",
            "GETs served from the LRU read cache.",
        )
        .expect("register read_cache_hits");
        registry.register(Box::new(read_cache_hits.clone())).ok();

        let read_cache_misses = IntCounter::new(
            "abixio_read_cache_misses_total",
            "GETs that missed the LRU read cache and went to disk.",
        )
        .expect("register read_cache_misses");
        registry.register(Box::new(read_cache_misses.clone())).ok();

        let write_cache_bytes = IntGauge::new(
            "abixio_write_cache_bytes",
            "Bytes held in the write cache.",
        )
        .expect("register write_cache_bytes");
        registry.register(Box::new(write_cache_bytes.clone())).ok();

        let write_cache_entries = IntGauge::new(
            "abixio_write_cache_entries",
            "Entries in the write cache.",
        )
        .expect("register write_cache_entries");
        registry
            .register(Box::new(write_cache_entries.clone()))
            .ok();

        let read_cache_bytes = IntGauge::new(
            "abixio_read_cache_bytes",
            "Bytes held in the read cache.",
        )
        .expect("register read_cache_bytes");
        registry.register(Box::new(read_cache_bytes.clone())).ok();

        let read_cache_entries = IntGauge::new(
            "abixio_read_cache_entries",
            "Entries in the read cache.",
        )
        .expect("register read_cache_entries");
        registry.register(Box::new(read_cache_entries.clone())).ok();

        let wal_materialized = IntCounter::new(
            "abixio_wal_materialized_total",
            "WAL entries materialized to the file tier.",
        )
        .expect("register wal_materialized");
        registry.register(Box::new(wal_materialized.clone())).ok();

        let wal_materialize_failed = IntCounter::new(
            "abixio_wal_materialize_failed_total",
            "WAL materialize attempts that returned an error.",
        )
        .expect("register wal_materialize_failed");
        registry
            .register(Box::new(wal_materialize_failed.clone()))
            .ok();

        let wal_pending = IntGaugeVec::new(
            Opts::new(
                "abixio_wal_pending_entries",
                "Un-materialized entries in the WAL, per disk.",
            ),
            &["disk"],
        )
        .expect("register wal_pending");
        registry.register(Box::new(wal_pending.clone())).ok();

        let heal_repaired = IntCounter::new(
            "abixio_heal_repaired_total",
            "Objects repaired by the heal worker.",
        )
        .expect("register heal_repaired");
        registry.register(Box::new(heal_repaired.clone())).ok();

        let heal_unrecoverable = IntCounter::new(
            "abixio_heal_unrecoverable_total",
            "Objects the heal worker could not reconstruct.",
        )
        .expect("register heal_unrecoverable");
        registry
            .register(Box::new(heal_unrecoverable.clone()))
            .ok();

        let mrf_queue_depth = IntGauge::new(
            "abixio_mrf_queue_depth",
            "Pending heal requests in the MRF queue.",
        )
        .expect("register mrf_queue_depth");
        registry.register(Box::new(mrf_queue_depth.clone())).ok();

        let scanner_objects_checked = IntCounter::new(
            "abixio_scanner_objects_checked_total",
            "Objects inspected by the integrity scanner.",
        )
        .expect("register scanner_objects_checked");
        registry
            .register(Box::new(scanner_objects_checked.clone()))
            .ok();

        let scanner_last_pass_seconds = IntGauge::new(
            "abixio_scanner_last_pass_seconds",
            "Wall-clock seconds of the last scanner pass.",
        )
        .expect("register scanner_last_pass_seconds");
        registry
            .register(Box::new(scanner_last_pass_seconds.clone()))
            .ok();

        let lifecycle_deleted = IntCounter::new(
            "abixio_lifecycle_deleted_total",
            "Objects deleted (or delete-markered) by lifecycle rules.",
        )
        .expect("register lifecycle_deleted");
        registry.register(Box::new(lifecycle_deleted.clone())).ok();

        let lifecycle_deleted_versions = IntCounter::new(
            "abixio_lifecycle_deleted_versions_total",
            "Noncurrent versions deleted by lifecycle rules.",
        )
        .expect("register lifecycle_deleted_versions");
        registry
            .register(Box::new(lifecycle_deleted_versions.clone()))
            .ok();

        let lifecycle_delete_markers_reaped = IntCounter::new(
            "abixio_lifecycle_delete_markers_reaped_total",
            "Expired delete markers cleaned up by lifecycle rules.",
        )
        .expect("register lifecycle_delete_markers_reaped");
        registry
            .register(Box::new(lifecycle_delete_markers_reaped.clone()))
            .ok();

        let lifecycle_multipart_aborted = IntCounter::new(
            "abixio_lifecycle_multipart_aborted_total",
            "Incomplete multipart uploads aborted by lifecycle rules.",
        )
        .expect("register lifecycle_multipart_aborted");
        registry
            .register(Box::new(lifecycle_multipart_aborted.clone()))
            .ok();

        let lifecycle_last_pass_seconds = IntGauge::new(
            "abixio_lifecycle_last_pass_seconds",
            "Wall-clock seconds of the last lifecycle enforcement pass.",
        )
        .expect("register lifecycle_last_pass_seconds");
        registry
            .register(Box::new(lifecycle_last_pass_seconds.clone()))
            .ok();

        let disk_total_bytes = IntGaugeVec::new(
            Opts::new("abixio_disk_bytes_total", "Filesystem total bytes per disk."),
            &["disk"],
        )
        .expect("register disk_total_bytes");
        registry.register(Box::new(disk_total_bytes.clone())).ok();

        let disk_used_bytes = IntGaugeVec::new(
            Opts::new("abixio_disk_bytes_used", "Filesystem used bytes per disk."),
            &["disk"],
        )
        .expect("register disk_used_bytes");
        registry.register(Box::new(disk_used_bytes.clone())).ok();

        let disk_free_bytes = IntGaugeVec::new(
            Opts::new("abixio_disk_bytes_free", "Filesystem free bytes per disk."),
            &["disk"],
        )
        .expect("register disk_free_bytes");
        registry.register(Box::new(disk_free_bytes.clone())).ok();

        let cluster_state = IntGaugeVec::new(
            Opts::new(
                "abixio_cluster_state",
                "One gauge per state, 1 for current, 0 otherwise.",
            ),
            &["state"],
        )
        .expect("register cluster_state");
        registry.register(Box::new(cluster_state.clone())).ok();

        let cluster_reachable_voters = IntGauge::new(
            "abixio_cluster_reachable_voters",
            "Voters reachable from this node, including self.",
        )
        .expect("register cluster_reachable_voters");
        registry
            .register(Box::new(cluster_reachable_voters.clone()))
            .ok();

        let cluster_quorum = IntGauge::new(
            "abixio_cluster_quorum",
            "Voters required for quorum.",
        )
        .expect("register cluster_quorum");
        registry.register(Box::new(cluster_quorum.clone())).ok();

        let cluster_epoch_id = IntGauge::new(
            "abixio_cluster_epoch_id",
            "Current cluster epoch id.",
        )
        .expect("register cluster_epoch_id");
        registry.register(Box::new(cluster_epoch_id.clone())).ok();

        Arc::new(Self {
            registry,
            s3_requests,
            s3_request_duration,
            s3_bytes_sent,
            s3_bytes_received,
            write_cache_hits,
            write_cache_replicate_success,
            write_cache_replicate_fail,
            read_cache_hits,
            read_cache_misses,
            write_cache_bytes,
            write_cache_entries,
            read_cache_bytes,
            read_cache_entries,
            wal_materialized,
            wal_materialize_failed,
            wal_pending,
            heal_repaired,
            heal_unrecoverable,
            mrf_queue_depth,
            scanner_objects_checked,
            scanner_last_pass_seconds,
            lifecycle_deleted,
            lifecycle_deleted_versions,
            lifecycle_delete_markers_reaped,
            lifecycle_multipart_aborted,
            lifecycle_last_pass_seconds,
            disk_total_bytes,
            disk_used_bytes,
            disk_free_bytes,
            cluster_state,
            cluster_reachable_voters,
            cluster_quorum,
            cluster_epoch_id,
        })
    }

    /// Render the Prometheus text format body for the `/metrics`
    /// endpoint.
    pub fn render(&self) -> Vec<u8> {
        let encoder = TextEncoder::new();
        let families = self.registry.gather();
        let mut buf = Vec::with_capacity(8192);
        if let Err(e) = encoder.encode(&families, &mut buf) {
            tracing::error!(error = %e, "metrics encode failed");
            buf.clear();
        }
        buf
    }

    pub fn record_request(
        &self,
        op: &str,
        status: StatusClass,
        duration: Duration,
        bytes_in: u64,
        bytes_out: u64,
    ) {
        self.s3_requests
            .with_label_values(&[op, status.as_str()])
            .inc();
        self.s3_request_duration
            .with_label_values(&[op])
            .observe(duration.as_secs_f64());
        if bytes_in > 0 {
            self.s3_bytes_received
                .with_label_values(&[op])
                .inc_by(bytes_in);
        }
        if bytes_out > 0 {
            self.s3_bytes_sent
                .with_label_values(&[op])
                .inc_by(bytes_out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_produces_valid_text_format() {
        let m = Metrics::new();
        m.record_request("GetObject", StatusClass::Ok, Duration::from_millis(3), 0, 128);
        m.record_request("PutObject", StatusClass::ClientErr, Duration::from_millis(12), 50, 0);
        m.read_cache_hits.inc();
        m.heal_repaired.inc_by(3);

        let body = m.render();
        let s = String::from_utf8(body).unwrap();
        assert!(s.contains("abixio_s3_requests_total"));
        assert!(s.contains("abixio_s3_request_duration_seconds"));
        assert!(s.contains("abixio_read_cache_hits_total 1"));
        assert!(s.contains("abixio_heal_repaired_total 3"));
        // label escaping + exemplar-free lines end with integers
        assert!(s.contains("op=\"GetObject\""));
        assert!(s.contains("status_class=\"client_err\""));
    }

    #[test]
    fn status_class_mapping() {
        assert!(matches!(
            StatusClass::from_http_status(200),
            StatusClass::Ok
        ));
        assert!(matches!(
            StatusClass::from_http_status(404),
            StatusClass::ClientErr
        ));
        assert!(matches!(
            StatusClass::from_http_status(500),
            StatusClass::ServerErr
        ));
    }
}
