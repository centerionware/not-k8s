//! Small keyed, async work queue for event-driven controllers.
//!
//! Kubernetes controllers do not reconcile directly in a watch callback.
//! They enqueue an object key, coalesce duplicate notifications, and let a
//! bounded worker process the latest cached object. This is the important
//! distinction between "async" and "backpressured": an async loop can still
//! perform one network reconcile for every copy of a burst.

use std::collections::{HashSet, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;
use tokio::sync::Notify;

struct State<K> {
    pending: VecDeque<K>,
    queued: HashSet<K>,
}

/// A deduplicating FIFO queue. `enqueue` is non-blocking; `pop` waits
/// asynchronously when there is no work. A key is removed from the queued
/// set when popped, so an event that arrives while that key is being
/// reconciled schedules exactly one follow-up pass.
pub struct KeyedWorkQueue<K> {
    state: Mutex<State<K>>,
    notify: Notify,
}

impl<K> Default for KeyedWorkQueue<K> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                pending: VecDeque::new(),
                queued: HashSet::new(),
            }),
            notify: Notify::new(),
        }
    }
}

impl<K> KeyedWorkQueue<K>
where
    K: Clone + Eq + Hash,
{
    pub fn enqueue(&self, key: K) {
        let mut state = self.state.lock().expect("work queue mutex poisoned");
        if state.queued.insert(key.clone()) {
            state.pending.push_back(key);
            self.notify.notify_one();
        }
    }

    pub async fn pop(&self) -> K {
        loop {
            // Issue #528's JoinSet refactor needs every controller's run()
            // to be Send (tokio::task::JoinSet::spawn's own bound) --
            // try_join! never required that, since it polls every future
            // on one task rather than moving them to spawned ones. A
            // std::sync::MutexGuard isn't Send, and an explicit drop()
            // right before the .await below (this function's previous
            // shape) still left rustc's async-fn state-machine transform
            // conservatively including it in the generator's layout for
            // the whole loop iteration in this shape -- a real, known
            // limitation, not a logic bug in the previous code. A block
            // that lexically ends the guard's scope, rather than a
            // runtime drop() call, is the reliable fix: the compiler can
            // then prove `state` is not live across the suspend point
            // from scoping alone.
            let popped = {
                let mut state = self.state.lock().expect("work queue mutex poisoned");
                let key = state.pending.pop_front();
                if let Some(key) = &key {
                    state.queued.remove(key);
                }
                key
            };
            if let Some(key) = popped {
                return key;
            }
            self.notify.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KeyedWorkQueue;

    #[tokio::test]
    async fn coalesces_duplicate_keys_until_the_worker_pops_them() {
        let queue = KeyedWorkQueue::default();
        queue.enqueue("pvc-a");
        queue.enqueue("pvc-a");
        assert_eq!(queue.pop().await, "pvc-a");
    }

    #[tokio::test]
    async fn an_event_during_reconcile_schedules_one_follow_up() {
        let queue = KeyedWorkQueue::default();
        queue.enqueue("pvc-a");
        assert_eq!(queue.pop().await, "pvc-a");
        queue.enqueue("pvc-a");
        queue.enqueue("pvc-a");
        assert_eq!(queue.pop().await, "pvc-a");
    }
}
