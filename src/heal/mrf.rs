use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// An MRF (Most Recently Failed) entry: an object that needs healing.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct MrfEntry {
    pub bucket: String,
    pub key: String,
}

/// MRF queue for immediate healing of partially-failed writes.
pub struct MrfQueue {
    tx: mpsc::Sender<MrfEntry>,
    rx: Mutex<Option<mpsc::Receiver<MrfEntry>>>,
    seen: Arc<Mutex<HashSet<MrfEntry>>>,
    _capacity: usize,
}

impl MrfQueue {
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            seen: Arc::new(Mutex::new(HashSet::new())),
            _capacity: capacity,
        }
    }

    /// Enqueue an entry for healing. Deduplicates: if the same (bucket, key)
    /// is already pending, this is a no-op.
    pub fn enqueue(&self, entry: MrfEntry) -> Result<(), String> {
        let mut seen = self.seen.lock().unwrap();
        if seen.contains(&entry) {
            return Ok(()); // already pending
        }
        match self.tx.try_send(entry.clone()) {
            Ok(()) => {
                seen.insert(entry);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("MRF queue full, dropping entry");
                Err("queue full".to_string())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err("queue closed".to_string()),
        }
    }

    /// Take the receiver half (for the worker to drain).
    /// Can only be called once.
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<MrfEntry>> {
        self.rx.lock().unwrap().take()
    }

    /// Remove an entry from the seen set after it's been healed.
    pub fn mark_done(&self, entry: &MrfEntry) {
        self.seen.lock().unwrap().remove(entry);
    }

    pub fn pending_count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_and_dequeue() {
        let queue = MrfQueue::new(10);
        let entry = MrfEntry {
            bucket: "test".to_string(),
            key: "key".to_string(),
        };
        queue.enqueue(entry.clone()).unwrap();
        assert_eq!(queue.pending_count(), 1);
    }

    #[test]
    fn dedup_same_entry() {
        let queue = MrfQueue::new(10);
        let entry = MrfEntry {
            bucket: "test".to_string(),
            key: "key".to_string(),
        };
        queue.enqueue(entry.clone()).unwrap();
        queue.enqueue(entry.clone()).unwrap(); // should be no-op
        assert_eq!(queue.pending_count(), 1);
    }

    #[test]
    fn queue_full_drops() {
        let queue = MrfQueue::new(1);
        let e1 = MrfEntry {
            bucket: "test".to_string(),
            key: "k1".to_string(),
        };
        let e2 = MrfEntry {
            bucket: "test".to_string(),
            key: "k2".to_string(),
        };
        queue.enqueue(e1).unwrap();
        let result = queue.enqueue(e2);
        assert!(result.is_err());
    }

    #[test]
    fn mark_done_allows_re_enqueue() {
        let queue = MrfQueue::new(10);
        let entry = MrfEntry {
            bucket: "test".to_string(),
            key: "key".to_string(),
        };
        queue.enqueue(entry.clone()).unwrap();
        queue.mark_done(&entry);
        assert_eq!(queue.pending_count(), 0);
        queue.enqueue(entry).unwrap();
        assert_eq!(queue.pending_count(), 1);
    }
}
