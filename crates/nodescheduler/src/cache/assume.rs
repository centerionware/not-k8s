//! Assumed pods — the optimistic reservation that lets the scheduling loop
//! run ahead of the (slow, asynchronous) binding cycle.
//!
//! # What "assume" buys
//!
//! Binding is an apiserver round trip, and everything after it — the apiserver
//! writing the pod, the watch delivering it back — takes tens of milliseconds
//! at best. If the scheduler waited for that before considering the next pod,
//! throughput would be capped at the apiserver's latency. If it *didn't* wait
//! and also didn't record anything, it would place the next pod as though the
//! previous one did not exist, and both would land on the same node's last
//! free CPU.
//!
//! So at the end of a successful scheduling cycle the pod is *assumed*: marked
//! as running on the chosen node and committed to that node's totals
//! immediately, before any API call. Subsequent cycles see the capacity as
//! already spent. The reservation is released only by
//! [`AssumedPods::forget`] (the binding failed) or superseded when the
//! informer delivers the real bound pod.
//!
//! # Assumed pods never expire
//!
//! Upstream used to expire them on a TTL, with a 1-second ticker sweeping for
//! stale entries. That was removed (k/k#106361) because it double-booked
//! resources: under a slow apiserver the TTL fired while the bind was still in
//! flight, the reservation was dropped, another pod took the capacity, and
//! then the original bind succeeded. Both pods ended up on a node sized for
//! one.
//!
//! So there is no TTL here and no sweep — which is also one of the three idle
//! timers docs/SCHEDULER.md commits to not having. The invariant that replaces
//! it is simpler and stronger: **every path out of the binding cycle must call
//! either `forget` or `finish_binding`.** A leaked assumption makes a node
//! permanently look fuller than it is, so `binder.rs` treats that pairing the
//! way it treats releasing a lock.

use super::pod::PodInfo;
use std::collections::HashMap;
use std::sync::Arc;

/// One optimistic reservation.
#[derive(Clone, Debug)]
pub struct Assumed {
    pub pod: Arc<PodInfo>,
    pub node_name: String,
    /// False while the binding cycle is still running. Kept for diagnostics
    /// only — nothing expires on it (see the module header), but "how many
    /// pods are mid-bind" is the first thing worth knowing when placement
    /// appears stalled.
    pub binding_finished: bool,
}

/// The set of pods this scheduler has placed but not yet seen confirmed.
#[derive(Debug, Default)]
pub struct AssumedPods {
    by_uid: HashMap<String, Assumed>,
}

impl AssumedPods {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the reservation. Returns the pod with `node_name` set, which is
    /// what the caller commits to the cache.
    ///
    /// The pod is copied rather than mutated in place because the queue and
    /// the watch layer may both still be holding the original, and a pod that
    /// silently acquired a `node_name` under them would look to the queue like
    /// something it should never have been handed.
    pub fn assume(&mut self, pod: &PodInfo, node_name: &str) -> Arc<PodInfo> {
        let mut placed = pod.clone();
        placed.node_name = Some(node_name.to_string());
        let placed = Arc::new(placed);

        self.by_uid.insert(
            pod.uid.clone(),
            Assumed {
                pod: placed.clone(),
                node_name: node_name.to_string(),
                binding_finished: false,
            },
        );
        placed
    }

    /// Release a reservation whose binding failed. Returns what to un-commit.
    ///
    /// Idempotent: a double-forget is a no-op rather than an error, because
    /// the failure paths in the binding cycle overlap and making them
    /// mutually exclusive is more fragile than making this tolerant.
    pub fn forget(&mut self, uid: &str) -> Option<Assumed> {
        self.by_uid.remove(uid)
    }

    /// The binding call returned successfully. The reservation stays until the
    /// informer delivers the real pod — that is what makes this safe against a
    /// bind that succeeded server-side but whose response we never saw.
    pub fn finish_binding(&mut self, uid: &str) {
        if let Some(a) = self.by_uid.get_mut(uid) {
            a.binding_finished = true;
        }
    }

    /// The informer delivered the real bound pod, so the assumption has been
    /// superseded by fact.
    pub fn confirmed(&mut self, uid: &str) -> Option<Assumed> {
        self.by_uid.remove(uid)
    }

    pub fn is_assumed(&self, uid: &str) -> bool {
        self.by_uid.contains_key(uid)
    }

    pub fn get(&self, uid: &str) -> Option<&Assumed> {
        self.by_uid.get(uid)
    }

    /// How many reservations are still waiting on their binding cycle.
    pub fn in_flight(&self) -> usize {
        self.by_uid.values().filter(|a| !a.binding_finished).count()
    }

    pub fn len(&self) -> usize {
        self.by_uid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_uid.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(uid: &str) -> PodInfo {
        PodInfo { uid: uid.to_string(), name: uid.to_string(), ..Default::default() }
    }

    #[test]
    fn assuming_a_pod_places_it_without_mutating_the_original() {
        let original = pod("p");
        let mut assumed = AssumedPods::new();

        let placed = assumed.assume(&original, "worker-1");

        assert_eq!(placed.node_name.as_deref(), Some("worker-1"));
        assert_eq!(original.node_name, None, "the queue's copy must be untouched");
    }

    #[test]
    fn an_assumed_pod_is_visible_immediately() {
        // This is the whole point: the next cycle must see the capacity spent
        // before any API call has happened.
        let mut assumed = AssumedPods::new();
        assumed.assume(&pod("p"), "worker-1");
        assert!(assumed.is_assumed("p"));
        assert_eq!(assumed.get("p").unwrap().node_name, "worker-1");
    }

    #[test]
    fn forgetting_releases_the_reservation_and_says_what_to_un_commit() {
        let mut assumed = AssumedPods::new();
        assumed.assume(&pod("p"), "worker-1");

        let released = assumed.forget("p").expect("p was assumed");

        assert_eq!(released.node_name, "worker-1");
        assert!(!assumed.is_assumed("p"));
    }

    #[test]
    fn forgetting_twice_is_not_an_error() {
        // The binding cycle's failure paths overlap; making them mutually
        // exclusive is more fragile than making this tolerant.
        let mut assumed = AssumedPods::new();
        assumed.assume(&pod("p"), "worker-1");
        assert!(assumed.forget("p").is_some());
        assert!(assumed.forget("p").is_none());
    }

    #[test]
    fn finishing_a_binding_does_not_release_the_reservation() {
        // It is held until the informer confirms. That is what protects
        // against a bind that succeeded server-side but whose response was
        // lost — releasing on the API call returning would hand the capacity
        // away while the pod really is running.
        let mut assumed = AssumedPods::new();
        assumed.assume(&pod("p"), "worker-1");

        assumed.finish_binding("p");

        assert!(assumed.is_assumed("p"), "still reserved until the watch confirms");
        assert_eq!(assumed.in_flight(), 0, "but no longer counted as mid-bind");
    }

    #[test]
    fn confirmation_by_the_informer_supersedes_the_assumption() {
        let mut assumed = AssumedPods::new();
        assumed.assume(&pod("p"), "worker-1");
        assumed.finish_binding("p");

        assert!(assumed.confirmed("p").is_some());
        assert!(!assumed.is_assumed("p"));
    }

    #[test]
    fn in_flight_counts_only_bindings_still_running() {
        let mut assumed = AssumedPods::new();
        assumed.assume(&pod("a"), "n1");
        assumed.assume(&pod("b"), "n1");
        assumed.finish_binding("a");

        assert_eq!(assumed.len(), 2);
        assert_eq!(assumed.in_flight(), 1);
    }

    #[test]
    fn re_assuming_a_pod_replaces_rather_than_duplicates_its_reservation() {
        let mut assumed = AssumedPods::new();
        assumed.assume(&pod("p"), "worker-1");
        assumed.assume(&pod("p"), "worker-2");

        assert_eq!(assumed.len(), 1);
        assert_eq!(assumed.get("p").unwrap().node_name, "worker-2");
    }
}
