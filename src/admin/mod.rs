pub mod handlers;
pub mod types;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Shared heal/scan statistics, updated by the scanner and MRF workers.
pub struct HealStats {
    pub objects_scanned: AtomicU64,
    pub objects_healed: AtomicU64,
    last_scan_started: Mutex<Option<Instant>>,
    last_scan_duration_secs: AtomicU64,
    pub started_at: Instant,
}

impl HealStats {
    pub fn new() -> Self {
        Self {
            objects_scanned: AtomicU64::new(0),
            objects_healed: AtomicU64::new(0),
            last_scan_started: Mutex::new(None),
            last_scan_duration_secs: AtomicU64::new(0),
            started_at: Instant::now(),
        }
    }

    pub fn record_scan(&self) {
        self.objects_scanned.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_heal(&self) {
        self.objects_healed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn scan_started(&self) {
        *self.last_scan_started.lock().unwrap() = Some(Instant::now());
    }

    pub fn scan_finished(&self) {
        if let Some(start) = *self.last_scan_started.lock().unwrap() {
            self.last_scan_duration_secs
                .store(start.elapsed().as_secs(), Ordering::Relaxed);
        }
    }

    pub fn last_scan_duration_secs(&self) -> u64 {
        self.last_scan_duration_secs.load(Ordering::Relaxed)
    }

    pub fn last_scan_started_secs_ago(&self) -> Option<u64> {
        self.last_scan_started
            .lock()
            .unwrap()
            .map(|s| s.elapsed().as_secs())
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}
