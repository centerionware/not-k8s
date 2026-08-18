//! Event-driven completion for `VolumeBinding`'s PreBind phase.
//!
//! The PVC informer is already the authoritative stream for scheduler cache
//! updates. Binding tasks subscribe here before their initial API read, then
//! wait for that stream to publish the PV-binder completion annotation. This
//! removes the former GET-every-two-seconds loop without creating a lost-wake
//! race between the initial read and the watch event.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
}

struct Entry {
    tx: watch::Sender<bool>,
    waiters: usize,
}

/// Shared by the PVC watch and every profile's VolumeBinding plugin.
#[derive(Clone, Default)]
pub struct PvcBindingTracker {
    inner: Arc<Mutex<Inner>>,
}

impl PvcBindingTracker {
    /// Subscribe before reading the PVC from the API. If binding completes
    /// between those two operations, the receiver retains the notification.
    pub fn subscribe(&self, key: String) -> PvcBindingWaiter {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.entries.entry(key.clone()).or_insert_with(|| {
            let (tx, _rx) = watch::channel(false);
            Entry { tx, waiters: 0 }
        });
        entry.waiters += 1;
        let rx = entry.tx.subscribe();
        PvcBindingWaiter { key, tracker: self.clone(), rx }
    }

    /// Publish the newest informer state. PVCs with no binding task waiting
    /// allocate nothing here; the ordinary scheduler cache still mirrors
    /// them as usual.
    pub fn observe(&self, key: &str, fully_bound: bool) {
        if let Some(entry) = self.inner.lock().unwrap().entries.get(key) {
            entry.tx.send_replace(fully_bound);
        }
    }

    fn release(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap();
        let remove = if let Some(entry) = inner.entries.get_mut(key) {
            entry.waiters = entry.waiters.saturating_sub(1);
            entry.waiters == 0
        } else {
            false
        };
        if remove {
            inner.entries.remove(key);
        }
    }

    #[cfg(test)]
    fn tracked_len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }
}

pub struct PvcBindingWaiter {
    key: String,
    tracker: PvcBindingTracker,
    rx: watch::Receiver<bool>,
}

impl PvcBindingWaiter {
    /// Wait until the informer observes a fully bound claim. The timeout is a
    /// failure bound, not polling: no task wakes before either a PVC event or
    /// the deadline.
    pub async fn wait(mut self, timeout: Duration) -> bool {
        let completed = async {
            loop {
                if *self.rx.borrow_and_update() {
                    return true;
                }
                if self.rx.changed().await.is_err() {
                    return false;
                }
            }
        };
        tokio::time::timeout(timeout, completed).await.unwrap_or(false)
    }
}

impl Drop for PvcBindingWaiter {
    fn drop(&mut self) {
        self.tracker.release(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_event_between_subscribe_and_wait_is_not_lost() {
        let tracker = PvcBindingTracker::default();
        let waiter = tracker.subscribe("ns/claim".to_string());
        tracker.observe("ns/claim", true);
        assert!(waiter.wait(Duration::from_millis(10)).await);
        assert_eq!(tracker.tracked_len(), 0);
    }

    #[tokio::test]
    async fn a_timeout_releases_the_tracker_entry() {
        let tracker = PvcBindingTracker::default();
        let waiter = tracker.subscribe("ns/claim".to_string());
        assert!(!waiter.wait(Duration::from_millis(1)).await);
        assert_eq!(tracker.tracked_len(), 0);
    }
}
