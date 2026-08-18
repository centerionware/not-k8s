//! Backoff: how long a pod waits after a wasted cycle, and where it waits.
//!
//! # Why the ceiling is 10 seconds and not 10 minutes
//!
//! Backoff here is not a circuit breaker. The queue is event-driven, so a pod
//! that cannot be placed costs nothing while it sits in `unschedulablePods` —
//! it is woken by a cluster event that a plugin said could un-stick it, not by
//! a retry loop. Backoff exists only to charge a pod for the *cycle it just
//! wasted*, so a pod that fails instantly cannot spin the scheduler. Upstream
//! therefore picks 1s doubling to 10s, which is deliberately aggressive: the
//! worst case of being too short is one extra cycle, and the worst case of
//! being too long is a placement delayed for no reason.
//!
//! Contrast `nodelet`'s pod retry (5s → 5m ceiling), which is the opposite
//! situation: there the retry *is* the only wake-up, so the ceiling has to buy
//! back the cost of polling forever. Copying that ceiling here would add
//! seconds of latency to every ordinary "the cluster was momentarily full"
//! case.
//!
//! # The divergence: no 1-second ticker
//!
//! Upstream flushes this structure from `flushBackoffQCompleted`, a
//! `time.Ticker` at 1Hz that runs whether or not anything is waiting. That is
//! one of the three unconditional idle timers docs/SCHEDULER.md commits to
//! removing, and it is pure waste: the heap already knows exactly when its
//! earliest entry comes due.
//!
//! So [`BackoffQueue`] exposes [`BackoffQueue::next_expiry`] and nothing
//! ticks. The run loop sleeps with `tokio::time::sleep_until` on that instant
//! and rearms whenever a push produces an earlier one. Semantics are
//! identical — a pod becomes active at its expiry, not up to a second late —
//! and an idle scheduler with an empty backoff queue wakes zero times.
//!
//! The rearm-on-push half is not optional. A `sleep_until` armed for the old
//! earliest expiry, with a nearer entry pushed after it, would leave that
//! entry sitting past its deadline until the older one fired. That is the one
//! way this structure can strand a pod, which is why `next_expiry` is checked
//! by the run loop after every mutation rather than only when a sleep expires.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::cache::PodInfo;

/// Upstream's `podInitialBackoffDuration`.
pub const DEFAULT_POD_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Upstream's `podMaxBackoffDuration`. Seconds — see the module header.
pub const DEFAULT_POD_MAX_BACKOFF: Duration = Duration::from_secs(10);

/// `initial * 2^(attempts-1)`, capped at `max`.
///
/// Pure, and split out from [`BackoffQueue`] for the same reason
/// `nodelet::pods::next_retry_delay` is split out of its retry task: the
/// schedule is the part worth asserting, and asserting it should not cost real
/// seconds of test time.
///
/// `attempts` is the number of scheduling attempts already made, so the first
/// failure (`attempts == 1`) waits `initial`. Zero is treated as one rather
/// than as "no wait", because a pod arriving here has by definition just used
/// a cycle. The shift is bounded before it happens: a pod that has failed 40
/// times must land on `max`, not wrap around to a near-zero delay.
pub fn backoff_duration(attempts: u32, initial: Duration, max: Duration) -> Duration {
    let shift = attempts.saturating_sub(1);
    if shift >= 32 {
        return max;
    }
    match initial.checked_mul(1u32 << shift) {
        Some(d) if d < max => d,
        _ => max,
    }
}

/// A pod waiting out its penalty, and when it comes due.
///
/// `Ord` is deliberately reversed so the standard max-heap yields the
/// *earliest* expiry, and `seq` breaks ties so two pods that come due in the
/// same instant still leave in the order they arrived rather than in whatever
/// order the heap happens to produce.
struct Entry {
    expiry: Instant,
    seq: u64,
    pod: Arc<PodInfo>,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.expiry == other.expiry && self.seq == other.seq
    }
}

impl Eq for Entry {}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .expiry
            .cmp(&self.expiry)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The pods serving backoff, ordered by when they come due.
///
/// Holds no timer of its own — see the module header. It is a plain data
/// structure and every method takes the time it should reason about, so the
/// tests never sleep and the run loop owns all the clock reading.
pub struct BackoffQueue {
    heap: BinaryHeap<Entry>,
    initial: Duration,
    max: Duration,
    next_seq: u64,
}

impl BackoffQueue {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            heap: BinaryHeap::new(),
            initial,
            max,
            next_seq: 0,
        }
    }

    /// The delay this queue would charge a pod on its `attempts`-th failure.
    pub fn delay_for(&self, attempts: u32) -> Duration {
        backoff_duration(attempts, self.initial, self.max)
    }

    /// Charge `pod` for a failed attempt, relative to `now`.
    pub fn push(&mut self, pod: Arc<PodInfo>, attempts: u32, now: Instant) {
        let expiry = now + self.delay_for(attempts);
        self.push_at(pod, expiry);
    }

    /// Enqueue with an expiry chosen by the caller. The seam the tests use to
    /// exercise ordering without waiting.
    pub fn push_at(&mut self, pod: Arc<PodInfo>, expiry: Instant) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.heap.push(Entry { expiry, seq, pod });
    }

    /// When the earliest-due pod comes due, or `None` if nothing is waiting.
    ///
    /// This is the whole reason the 1Hz flush ticker does not exist: the run
    /// loop sleeps to exactly this instant and rearms whenever a push moves
    /// it earlier.
    pub fn next_expiry(&self) -> Option<Instant> {
        self.heap.peek().map(|e| e.expiry)
    }

    /// Everything due at or before `now`, in expiry order.
    pub fn pop_expired(&mut self, now: Instant) -> Vec<Arc<PodInfo>> {
        let mut out = Vec::new();
        while let Some(e) = self.heap.peek() {
            if e.expiry > now {
                break;
            }
            // `peek` said there is one, so this cannot be None.
            if let Some(e) = self.heap.pop() {
                out.push(e.pod);
            }
        }
        out
    }

    /// Pull one specific pod out, for forced activation or deletion.
    ///
    /// O(n) and a full rebuild, because a binary heap has no cheap arbitrary
    /// removal and this path is rare — `Activate` and pod deletion, never the
    /// steady state.
    pub fn remove(&mut self, uid: &str) -> Option<Arc<PodInfo>> {
        let mut found = None;
        let rest: Vec<Entry> = std::mem::take(&mut self.heap)
            .into_vec()
            .into_iter()
            .filter(|e| {
                if e.pod.uid == uid {
                    found = Some(e.pod.clone());
                    false
                } else {
                    true
                }
            })
            .collect();
        self.heap = rest.into_iter().collect();
        found
    }

    pub fn contains(&self, uid: &str) -> bool {
        self.heap.iter().any(|e| e.pod.uid == uid)
    }

    /// Replace a pod's API projection without changing its deadline.
    ///
    /// Labels, requests, affinity, gates, and PVC references are scheduling
    /// inputs, so an edit received during backoff must be visible to the next
    /// cycle. The queue-owned fairness and attempt fields stay with the old
    /// entry, while its heap key stays byte-for-byte unchanged.
    pub fn update(&mut self, updated: &Arc<PodInfo>) -> bool {
        if !self.contains(&updated.uid) {
            return false;
        }
        let mut found = false;
        let entries: Vec<Entry> = std::mem::take(&mut self.heap)
            .into_vec()
            .into_iter()
            .map(|mut entry| {
                if entry.pod.uid == updated.uid {
                    let mut merged = (**updated).clone();
                    merged.queued_at = entry.pod.queued_at;
                    merged.attempts = entry.pod.attempts;
                    entry.pod = Arc::new(merged);
                    found = true;
                }
                entry
            })
            .collect();
        self.heap = entries.into_iter().collect();
        found
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::PodInfo;

    fn test_pod(uid: &str, priority: i32, _attempts: u32) -> std::sync::Arc<PodInfo> {
        std::sync::Arc::new(PodInfo {
            uid: uid.to_string(),
            name: uid.to_string(),
            priority,
            ..Default::default()
        })
    }

    #[test]
    fn the_backoff_sequence_doubles_from_one_second_and_settles_at_ten() {
        let (i, m) = (DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF);
        let seq: Vec<u64> = (1..=6)
            .map(|a| backoff_duration(a, i, m).as_secs())
            .collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 10, 10]);
    }

    #[test]
    fn a_pod_that_has_failed_absurdly_often_still_waits_exactly_the_ceiling() {
        // The shift must be bounded before it is applied; an overflow here
        // would wrap to a near-zero delay and turn a permanently broken pod
        // into a spin loop.
        let (i, m) = (DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF);
        for attempts in [31, 32, 33, 1000, u32::MAX] {
            assert_eq!(backoff_duration(attempts, i, m), m, "attempts={attempts}");
        }
    }

    #[test]
    fn the_first_attempt_waits_the_initial_delay_and_never_less() {
        let (i, m) = (DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF);
        assert_eq!(backoff_duration(0, i, m), i);
        assert_eq!(backoff_duration(1, i, m), i);
    }

    #[test]
    fn an_initial_delay_longer_than_the_ceiling_is_still_capped() {
        let d = backoff_duration(1, Duration::from_secs(30), Duration::from_secs(10));
        assert_eq!(d, Duration::from_secs(10));
    }

    #[test]
    fn next_expiry_is_the_earliest_entry_not_the_most_recently_pushed() {
        let mut q = BackoffQueue::new(DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF);
        assert_eq!(q.next_expiry(), None, "an empty queue arms no timer at all");

        let base = Instant::now();
        q.push_at(test_pod("late", 0, 0), base + Duration::from_secs(9));
        let far = q.next_expiry();
        q.push_at(test_pod("soon", 0, 0), base + Duration::from_secs(1));

        assert_ne!(
            q.next_expiry(),
            far,
            "a nearer push must move the deadline, or the run loop sleeps past it"
        );
        assert_eq!(q.next_expiry(), Some(base + Duration::from_secs(1)));
    }

    #[test]
    fn only_pods_whose_penalty_has_elapsed_come_out_and_they_come_out_in_order() {
        let mut q = BackoffQueue::new(DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF);
        let base = Instant::now();
        q.push_at(test_pod("third", 0, 0), base + Duration::from_secs(8));
        q.push_at(test_pod("first", 0, 0), base + Duration::from_secs(1));
        q.push_at(test_pod("second", 0, 0), base + Duration::from_secs(2));

        let due = q.pop_expired(base + Duration::from_secs(5));
        let names: Vec<&str> = due.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
        assert_eq!(q.len(), 1);
        assert_eq!(q.next_expiry(), Some(base + Duration::from_secs(8)));
    }

    #[test]
    fn two_pods_due_in_the_same_instant_leave_in_arrival_order() {
        let mut q = BackoffQueue::new(DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF);
        let at = Instant::now() + Duration::from_secs(1);
        q.push_at(test_pod("a", 0, 0), at);
        q.push_at(test_pod("b", 0, 0), at);
        q.push_at(test_pod("c", 0, 0), at);

        let names: Vec<String> = q
            .pop_expired(at)
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn an_update_replaces_the_pod_without_moving_its_deadline() {
        let mut q = BackoffQueue::new(DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF);
        let at = Instant::now() + Duration::from_secs(7);
        let mut original = (*test_pod("p", 1, 0)).clone();
        original.attempts = 4;
        q.push_at(Arc::new(original), at);

        let updated = test_pod("p", 99, 0);
        assert!(q.update(&updated));
        assert_eq!(q.next_expiry(), Some(at));

        let popped = q.pop_expired(at);
        assert_eq!(popped[0].priority, 99);
        assert_eq!(popped[0].attempts, 4);
    }

    #[test]
    fn removing_a_pod_leaves_the_rest_of_the_heap_correctly_ordered() {
        let mut q = BackoffQueue::new(DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF);
        let base = Instant::now();
        for (i, n) in ["a", "b", "c", "d"].iter().enumerate() {
            q.push_at(test_pod(n, 0, 0), base + Duration::from_secs(i as u64 + 1));
        }
        let taken = q.remove(&test_pod("b", 0, 0).uid).expect("b was queued");
        assert_eq!(taken.name, "b");
        assert!(!q.contains(&test_pod("b", 0, 0).uid));

        let names: Vec<String> = q
            .pop_expired(base + Duration::from_secs(60))
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(names, vec!["a", "c", "d"]);
    }

    #[test]
    fn removing_a_pod_that_is_not_queued_is_not_an_error() {
        let mut q = BackoffQueue::new(DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF);
        q.push_at(test_pod("a", 0, 0), Instant::now());
        assert!(q.remove("no-such-uid").is_none());
        assert_eq!(q.len(), 1);
    }
}
