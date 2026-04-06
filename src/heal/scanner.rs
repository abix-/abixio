use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Tracks per-object last-checked timestamps for cooldown.
pub struct ScanState {
    last_checked: Mutex<HashMap<String, Instant>>,
    heal_interval: Duration,
}

impl ScanState {
    pub fn new(heal_interval: Duration) -> Self {
        Self {
            last_checked: Mutex::new(HashMap::new()),
            heal_interval,
        }
    }

    /// Returns true if this object should be scanned (not checked recently).
    pub fn should_scan(&self, bucket: &str, key: &str) -> bool {
        let obj_key = format!("{}/{}", bucket, key);
        let map = match self.last_checked.lock() {
            Ok(guard) => guard,
            Err(e) => {
                tracing::error!("poisoned mutex (scanner should_scan): {}", e);
                return true; // scan if uncertain
            }
        };
        match map.get(&obj_key) {
            Some(last) => last.elapsed() >= self.heal_interval,
            None => true,
        }
    }

    /// Mark an object as just checked.
    pub fn mark_checked(&self, bucket: &str, key: &str) {
        let obj_key = format!("{}/{}", bucket, key);
        match self.last_checked.lock() {
            Ok(mut map) => { map.insert(obj_key, Instant::now()); }
            Err(e) => tracing::error!("poisoned mutex (scanner mark_checked): {}", e),
        }
    }

    /// Mark an object as checked at a specific instant (for testing).
    #[cfg(test)]
    pub fn mark_checked_at(&self, bucket: &str, key: &str, at: Instant) {
        let obj_key = format!("{}/{}", bucket, key);
        let mut map = self.last_checked.lock().unwrap();
        map.insert(obj_key, at);
    }

    pub fn checked_count(&self) -> usize {
        match self.last_checked.lock() {
            Ok(guard) => guard.len(),
            Err(e) => {
                tracing::error!("poisoned mutex (scanner checked_count): {}", e);
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_object_should_scan() {
        let state = ScanState::new(Duration::from_secs(3600));
        assert!(state.should_scan("bucket", "key"));
    }

    #[test]
    fn recently_checked_skips() {
        let state = ScanState::new(Duration::from_secs(3600));
        state.mark_checked("bucket", "key");
        assert!(!state.should_scan("bucket", "key"));
    }

    #[test]
    fn past_interval_rescans() {
        let state = ScanState::new(Duration::from_millis(10));
        // mark as checked in the past
        let past = Instant::now() - Duration::from_millis(100);
        state.mark_checked_at("bucket", "key", past);
        assert!(state.should_scan("bucket", "key"));
    }

    #[test]
    fn different_objects_independent() {
        let state = ScanState::new(Duration::from_secs(3600));
        state.mark_checked("bucket", "key1");
        assert!(!state.should_scan("bucket", "key1"));
        assert!(state.should_scan("bucket", "key2"));
    }
}
