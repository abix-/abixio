//! Request-path instrumentation helper. Holds nothing; the real state
//! lives in the `Metrics` registry. Exists so the dispatcher can call
//! a thin helper instead of threading label strings through ad-hoc.

use std::time::Duration;

use super::{Metrics, StatusClass};

/// Record one completed HTTP S3 request. `op` is the s3s operation
/// name (e.g. `"GetObject"`) or `"unknown"` if the request didn't
/// route through the S3 layer.
pub fn record_s3(
    metrics: &Metrics,
    op: &str,
    http_status: u16,
    duration: Duration,
    bytes_in: u64,
    bytes_out: u64,
) {
    let class = StatusClass::from_http_status(http_status);
    metrics.record_request(op, class, duration, bytes_in, bytes_out);
}
