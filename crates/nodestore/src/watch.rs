//! Watch fan-out — the reason this component exists.
//!
//! kine, which this replaces, drives watches by polling: every watcher's
//! changes are found by re-querying the table for rows past a remembered
//! revision, on a timer, forever. That is what shows up as a control plane
//! doing hundreds of syscalls a second on a cluster where nothing whatsoever
//! is happening. Here an applied command hands its events directly to the
//! watchers, and an idle store does nothing at all.
//!
//! # Delivery guarantee
//!
//! A watcher must never silently miss an event — apiserver's watch cache
//! trusts that a stream is gapless and only re-lists when *told* it can't be.
//! Two things threaten that, and both are handled by the same fallback:
//!
//!   * **Starting up.** A watch from a past revision has to replay history and
//!     then continue live, without dropping whatever arrives in between. The
//!     subscription is therefore taken *before* the replay query runs, and
//!     anything the replay already covered is discarded on the way through.
//!   * **Falling behind.** The broadcast buffer is bounded, so a slow watcher
//!     can be lagged off the end of it. That is not fatal: everything it
//!     missed is still in the store, so it re-reads from its last delivered
//!     revision and rejoins.
//!
//! The bounded buffer is deliberate. An unbounded one turns a stalled watcher
//! into unbounded memory growth in the datastore, which on an edge device is
//! the whole machine.

use crate::store::Event;
use std::sync::Arc;
use tokio::sync::broadcast;

/// One applied command's events, as broadcast to watchers.
#[derive(Clone, Debug)]
pub struct WatchBatch {
    pub revision: i64,
    pub events: Arc<Vec<Event>>,
}

/// Broadcasts applied events to every live watcher.
pub struct WatchHub {
    tx: broadcast::Sender<WatchBatch>,
}

impl WatchHub {
    pub fn new(capacity: usize) -> WatchHub {
        // broadcast requires a non-zero capacity, and a tiny one would lag
        // watchers constantly, turning the recovery path into the normal path.
        let (tx, _rx) = broadcast::channel(capacity.max(16));
        WatchHub { tx }
    }

    /// Subscribe before reading history — see the module note on why the order
    /// matters.
    pub fn subscribe(&self) -> broadcast::Receiver<WatchBatch> {
        self.tx.subscribe()
    }

    /// Publish one command's events. Errors are ignored on purpose: the only
    /// error is "nobody is listening", which is the common case for a store
    /// with no watchers and not a problem.
    pub fn publish(&self, revision: i64, events: Vec<Event>) {
        let _ = self.tx.send(WatchBatch { revision, events: Arc::new(events) });
    }

    /// Live watcher count. Diagnostics only.
    pub fn watcher_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EventKind, KeyValue};

    fn event(key: &str, rev: i64) -> Event {
        Event {
            kind: EventKind::Put,
            kv: KeyValue { key: key.as_bytes().to_vec(), mod_revision: rev, ..Default::default() },
            prev_kv: None,
        }
    }

    #[tokio::test]
    async fn a_subscriber_receives_published_batches() {
        let hub = WatchHub::new(16);
        let mut rx = hub.subscribe();
        hub.publish(2, vec![event("/a", 2)]);
        let batch = rx.recv().await.unwrap();
        assert_eq!(batch.revision, 2);
        assert_eq!(batch.events[0].kv.key, b"/a");
    }

    #[tokio::test]
    async fn publishing_with_no_subscribers_is_not_an_error() {
        // The ordinary state of an idle store. If this were treated as a
        // failure, every write on a watcher-less cluster would log one.
        let hub = WatchHub::new(16);
        hub.publish(2, vec![event("/a", 2)]);
        assert_eq!(hub.watcher_count(), 0);
    }

    #[tokio::test]
    async fn a_subscriber_only_sees_batches_published_after_it_subscribed() {
        // Which is exactly why the watch path subscribes before replaying
        // history rather than after.
        let hub = WatchHub::new(16);
        hub.publish(2, vec![event("/before", 2)]);
        let mut rx = hub.subscribe();
        hub.publish(3, vec![event("/after", 3)]);
        let batch = rx.recv().await.unwrap();
        assert_eq!(batch.revision, 3);
    }

    #[tokio::test]
    async fn a_slow_subscriber_is_lagged_rather_than_growing_the_buffer() {
        // The signal the recovery path keys on: RecvError::Lagged tells the
        // watcher it must re-read from the store instead of assuming its
        // stream is still gapless.
        let hub = WatchHub::new(16);
        let mut rx = hub.subscribe();
        for rev in 2..40 {
            hub.publish(rev, vec![event("/a", rev)]);
        }
        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => assert!(n > 0),
            other => panic!("expected a lag signal, got {other:?}"),
        }
    }
}
