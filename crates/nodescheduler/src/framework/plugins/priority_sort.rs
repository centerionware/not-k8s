//! `PrioritySort` — the only QueueSort plugin in the default profile.
//!
//! Higher `spec.priority` first; ties broken by whoever entered the queue
//! first.
//!
//! # The tiebreak is `queued_at`, not `creationTimestamp`
//!
//! A pod that fails a scheduling cycle is requeued, and it keeps its original
//! `queued_at`. Using creation time would be *almost* the same thing and would
//! also be fine — the bug is the third option, resetting the timestamp on
//! requeue, which is the natural thing to write and which starves exactly the
//! pods having the hardest time getting placed: every failure sends them to
//! the back of their priority band, behind pods that arrived later and will
//! succeed immediately. It shows up only under contention, and it looks like
//! unfairness rather than a bug.
//!
//! Only one QueueSort plugin may be enabled, and every profile must use the
//! same one: there is a single queue shared across profiles, so two orderings
//! is not a thing that can exist.

use crate::cache::PodInfo;
use crate::framework::{Plugin, QueueSortPlugin};

pub const NAME: &str = "PrioritySort";

pub struct PrioritySort;

impl Plugin for PrioritySort {
    fn name(&self) -> &'static str {
        NAME
    }
    // No events: ordering the queue cannot reject anything, so nothing it
    // does can strand a pod.
}

impl QueueSortPlugin for PrioritySort {
    fn less(&self, a: &PodInfo, b: &PodInfo) -> bool {
        if a.priority != b.priority {
            return a.priority > b.priority;
        }
        a.queued_at < b.queued_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn pod_at(name: &str, priority: i32, secs: i64) -> PodInfo {
        PodInfo {
            name: name.to_string(),
            priority,
            queued_at: Utc::now() + Duration::seconds(secs),
            ..Default::default()
        }
    }

    #[test]
    fn higher_priority_sorts_first() {
        let high = pod_at("high", 1000, 0);
        let low = pod_at("low", 0, 0);
        assert!(PrioritySort.less(&high, &low));
        assert!(!PrioritySort.less(&low, &high));
    }

    #[test]
    fn equal_priority_is_broken_by_who_queued_first() {
        let first = pod_at("first", 0, 0);
        let second = pod_at("second", 0, 10);
        assert!(PrioritySort.less(&first, &second));
        assert!(!PrioritySort.less(&second, &first));
    }

    #[test]
    fn priority_outranks_age() {
        // An old low-priority pod must not hold up a new high-priority one.
        let old_low = pod_at("old", 0, 0);
        let new_high = pod_at("new", 1000, 100);
        assert!(PrioritySort.less(&new_high, &old_low));
    }

    #[test]
    fn negative_priorities_sort_below_the_default() {
        let normal = pod_at("normal", 0, 0);
        let background = pod_at("background", -10, 0);
        assert!(PrioritySort.less(&normal, &background));
    }

    #[test]
    fn a_requeued_pod_keeps_its_place_in_its_priority_band() {
        // The starvation case from the module header, stated as a test: a pod
        // that has failed cycles must still precede one that queued later.
        let mut retried = pod_at("retried", 0, 0);
        retried.attempts = 5;
        let newcomer = pod_at("newcomer", 0, 10);

        assert!(
            PrioritySort.less(&retried, &newcomer),
            "failing cycles must not send a pod to the back of its band"
        );
    }
}
