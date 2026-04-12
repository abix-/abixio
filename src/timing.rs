//! Per-request layer timing.
//!
//! Captures how long each layer of a request spent (`dispatch`, `s3s_total`,
//! `setup`, `validate`, `ec_encode`, `storage_write`, `response_build`, ...)
//! and emits the result as a W3C `server-timing` HTTP response header
//! alongside the legacy `x-debug-s3s-ms` total field.
//!
//! Design: a `tokio::task_local!` static `REQUEST_TIMING` is set up by the
//! dispatch layer (`src/s3_route.rs`) once per incoming request via
//! `REQUEST_TIMING.scope(...)`. Anywhere inside the call tree, code can
//! call `timing::record(name, duration)` (or use the `Span` RAII guard or
//! the `time(name, future).await` helper) and the entry lands in the
//! per-task struct. When `s3s` returns to dispatch, the accumulated layers
//! get serialized into the response header.
//!
//! Background spawned tasks (rename worker, scanner, heal worker) do NOT
//! inherit the task-local. That's intentional -- they run after ack and
//! are not on the response path.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Per-request layer timing accumulator.
#[derive(Debug, Default)]
pub struct RequestTiming {
    layers: Vec<(&'static str, Duration)>,
}

impl RequestTiming {
    pub fn new() -> Self {
        Self {
            layers: Vec::with_capacity(8),
        }
    }

    pub fn record(&mut self, name: &'static str, dur: Duration) {
        self.layers.push((name, dur));
    }

    /// Format as a W3C `server-timing` header value:
    /// `name1;dur=1.234, name2;dur=5.678`. Millisecond resolution, 3 decimals.
    /// Returns the empty string if no layers were recorded.
    pub fn to_server_timing(&self) -> String {
        if self.layers.is_empty() {
            return String::new();
        }
        let mut s = String::with_capacity(self.layers.len() * 24);
        for (i, (name, dur)) in self.layers.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            // {name};dur={ms:.3}
            s.push_str(name);
            s.push_str(";dur=");
            // sub-millisecond precision matters for the small layers
            let ms = dur.as_secs_f64() * 1000.0;
            // SAFETY: write! into String never fails
            use std::fmt::Write as _;
            let _ = write!(s, "{:.3}", ms);
        }
        s
    }
}

tokio::task_local! {
    pub static REQUEST_TIMING: Arc<Mutex<RequestTiming>>;
}

/// Record a finished layer into the current request's timing struct.
/// No-op if called outside a `REQUEST_TIMING.scope(...)` (e.g. background tasks).
#[inline]
pub fn record(name: &'static str, dur: Duration) {
    let _ = REQUEST_TIMING.try_with(|t| {
        if let Ok(mut g) = t.lock() {
            g.record(name, dur);
        }
    });
}

/// Time a future and record the elapsed under `name`. Use this when the
/// section to time composes into a single async call:
///
/// ```ignore
/// let result = timing::time("storage", store.put_object_stream(...)).await;
/// ```
pub async fn time<F, T>(name: &'static str, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let start = Instant::now();
    let out = fut.await;
    record(name, start.elapsed());
    out
}

/// RAII timing guard. Records the elapsed under `name` when dropped.
/// Use this when the section to time is a sync block or several
/// statements that don't compose into a single future:
///
/// ```ignore
/// {
///     let _t = timing::Span::new("setup");
///     // ... work ...
/// } // <-- recorded here on drop
/// ```
pub struct Span {
    name: &'static str,
    start: Instant,
}

impl Span {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: Instant::now(),
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        record(self.name, self.start.elapsed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_renders_empty() {
        let t = RequestTiming::new();
        assert_eq!(t.to_server_timing(), "");
    }

    #[test]
    fn single_layer_renders() {
        let mut t = RequestTiming::new();
        t.record("storage", Duration::from_micros(265));
        assert_eq!(t.to_server_timing(), "storage;dur=0.265");
    }

    #[test]
    fn multi_layer_renders_in_order() {
        let mut t = RequestTiming::new();
        t.record("dispatch", Duration::from_micros(5));
        t.record("s3s_total", Duration::from_micros(1380));
        t.record("storage", Duration::from_micros(265));
        assert_eq!(
            t.to_server_timing(),
            "dispatch;dur=0.005, s3s_total;dur=1.380, storage;dur=0.265"
        );
    }

    #[tokio::test]
    async fn record_outside_scope_is_noop() {
        // No panic, no error.
        record("orphan", Duration::from_millis(1));
    }

    #[tokio::test]
    async fn record_inside_scope_lands_in_task_local() {
        let timing = Arc::new(Mutex::new(RequestTiming::new()));
        REQUEST_TIMING
            .scope(timing.clone(), async {
                record("a", Duration::from_micros(100));
                record("b", Duration::from_micros(200));
            })
            .await;
        let g = timing.lock().unwrap();
        assert_eq!(g.layers.len(), 2);
        assert_eq!(g.layers[0].0, "a");
        assert_eq!(g.layers[1].0, "b");
    }

    #[tokio::test]
    async fn span_records_on_drop() {
        let timing = Arc::new(Mutex::new(RequestTiming::new()));
        REQUEST_TIMING
            .scope(timing.clone(), async {
                let _s = Span::new("setup");
                tokio::time::sleep(Duration::from_millis(1)).await;
            })
            .await;
        let g = timing.lock().unwrap();
        assert_eq!(g.layers.len(), 1);
        assert_eq!(g.layers[0].0, "setup");
        assert!(g.layers[0].1 >= Duration::from_millis(1));
    }

    #[tokio::test]
    async fn time_helper_records() {
        let timing = Arc::new(Mutex::new(RequestTiming::new()));
        let result = REQUEST_TIMING
            .scope(timing.clone(), async {
                time("work", async { 42 }).await
            })
            .await;
        assert_eq!(result, 42);
        let g = timing.lock().unwrap();
        assert_eq!(g.layers.len(), 1);
        assert_eq!(g.layers[0].0, "work");
    }
}
