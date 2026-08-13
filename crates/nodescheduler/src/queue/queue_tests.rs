//! Tests for the scheduling queue.
//!
//! The in-flight replay tests are the ones that earn their keep: that bug is
//! invisible (a pod is simply slow), non-deterministic (it depends on event
//! timing against cycle duration), and self-healing after five minutes, so it
//! is almost impossible to catch any way other than deliberately.

use super::*;
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::ClusterEventWithHint;
use backoff::{BackoffQueue, DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF};

fn pod(uid: &str, priority: i32) -> Arc<PodInfo> {
    Arc::new(PodInfo {
        uid: uid.to_string(),
        name: uid.to_string(),
        priority,
        queued_at: k8s_openapi::jiff::Timestamp::now(),
        ..Default::default()
    })
}

fn node_added() -> ClusterEvent {
    ClusterEvent::new(EventResource::Node, ActionType::ADD)
}

/// A queue whose "Fit" plugin wakes on node additions, admitting everything.
fn queue() -> SchedulingQueue {
    queue_with_pre_enqueue(Arc::new(|_: &PodInfo| Status::success()))
}

fn queue_with_pre_enqueue(pre_enqueue: PreEnqueueFn) -> SchedulingQueue {
    let mut hints = HintRegistry::new();
    hints.register("Fit", vec![ClusterEventWithHint::always(node_added())]);

    SchedulingQueue::new(
        hints,
        Arc::new(|a: &PodInfo, b: &PodInfo| {
            if a.priority != b.priority {
                a.priority > b.priority
            } else {
                a.queued_at < b.queued_at
            }
        }),
        pre_enqueue,
        BackoffQueue::new(DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF),
        DEFAULT_MAX_IN_UNSCHEDULABLE,
    )
}

#[tokio::test]
async fn pods_come_out_in_priority_order() {
    let q = queue();
    q.add(pod("low", 0));
    q.add(pod("high", 1000));
    q.add(pod("mid", 500));

    assert_eq!(q.pop().await.uid, "high");
    assert_eq!(q.pop().await.uid, "mid");
    assert_eq!(q.pop().await.uid, "low");
}

#[tokio::test]
async fn pop_waits_rather_than_spinning_on_an_empty_queue() {
    let q = Arc::new(queue());
    let q2 = q.clone();

    let popped = tokio::spawn(async move { q2.pop().await.uid.clone() });
    // Nothing to take yet; the pop must be parked, not looping.
    tokio::task::yield_now().await;
    q.add(pod("late", 0));

    assert_eq!(popped.await.unwrap(), "late");
}

#[test]
fn a_pod_rejected_by_pre_enqueue_never_reaches_the_active_queue() {
    // A gated pod is held without being counted as a scheduling failure.
    let q = queue_with_pre_enqueue(Arc::new(|_: &PodInfo| {
        Status::unschedulable("SchedulingGates", "gated")
    }));
    q.add(pod("gated", 0));

    assert_eq!(q.active_len(), 0);
    assert_eq!(q.unschedulable_len(), 1);
}

#[test]
fn adding_the_same_pod_twice_does_not_duplicate_it() {
    let q = queue();
    q.add(pod("p", 0));
    q.add(pod("p", 0));
    assert_eq!(q.active_len(), 1);
}

#[test]
fn a_matching_event_moves_a_parked_pod_to_backoff() {
    let q = queue();
    q.add_unschedulable(pod("p", 0), vec!["Fit"], vec![]);
    assert_eq!(q.unschedulable_len(), 1);

    q.move_all_to_active_or_backoff(node_added(), None, None);

    assert_eq!(q.unschedulable_len(), 0);
    assert_eq!(q.backoff_len(), 1);
}

#[test]
fn an_unrelated_event_leaves_a_parked_pod_parked() {
    let q = queue();
    q.add_unschedulable(pod("p", 0), vec!["Fit"], vec![]);

    q.move_all_to_active_or_backoff(
        ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_LABEL),
        None,
        None,
    );

    assert_eq!(q.unschedulable_len(), 1);
    assert_eq!(q.backoff_len(), 0);
}

#[test]
fn a_pending_rejection_goes_straight_to_active_skipping_backoff() {
    let mut hints = HintRegistry::new();
    hints.register("DRA", vec![ClusterEventWithHint::always(node_added())]);
    let q = SchedulingQueue::new(
        hints,
        Arc::new(|a: &PodInfo, b: &PodInfo| a.priority > b.priority),
        Arc::new(|_: &PodInfo| Status::success()),
        BackoffQueue::new(DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF),
        DEFAULT_MAX_IN_UNSCHEDULABLE,
    );

    q.add_unschedulable(pod("p", 0), vec![], vec!["DRA"]);
    q.move_all_to_active_or_backoff(node_added(), None, None);

    assert_eq!(q.active_len(), 1, "progress was made, so no penalty is owed");
    assert_eq!(q.backoff_len(), 0);
}

#[test]
fn a_still_gated_pod_bounces_back_rather_than_entering_the_active_queue() {
    // An unrelated cluster change must not let a gated pod through.
    let q = queue_with_pre_enqueue(Arc::new(|_: &PodInfo| {
        Status::unschedulable("SchedulingGates", "gated")
    }));
    q.add_unschedulable(pod("gated", 0), vec!["Fit"], vec![]);

    q.move_all_to_active_or_backoff(node_added(), None, None);

    assert_eq!(q.active_len(), 0);
    assert_eq!(q.unschedulable_len(), 1);
}

// ── In-flight replay ────────────────────────────────────────────────────

#[tokio::test]
async fn an_event_arriving_mid_cycle_is_replayed_for_that_pod() {
    // THE test. Without the replay the pod parks and waits for the *next*
    // matching event, which on a quiet cluster never comes — the workload
    // simply never runs, and nothing anywhere reports an error.
    let q = queue();
    q.add(pod("p", 0));

    let popped = q.pop().await; // cycle starts
    q.move_all_to_active_or_backoff(node_added(), None, None); // arrives during it
    q.add_unschedulable(popped, vec!["Fit"], vec![]); // cycle rejects it

    assert_eq!(
        q.unschedulable_len(),
        0,
        "the event that arrived mid-cycle must not be lost"
    );
    assert_eq!(q.backoff_len(), 1);
}

#[tokio::test]
async fn an_event_arriving_before_the_cycle_started_is_not_replayed() {
    // It was already accounted for when it happened; replaying it would
    // retry the pod for a change its own cycle already saw.
    let q = queue();
    q.move_all_to_active_or_backoff(node_added(), None, None);
    q.add(pod("p", 0));

    let popped = q.pop().await;
    q.add_unschedulable(popped, vec!["Fit"], vec![]);

    assert_eq!(q.unschedulable_len(), 1);
}

#[tokio::test]
async fn an_event_is_replayed_only_for_cycles_it_actually_overlapped() {
    let q = queue();
    q.add(pod("first", 100));
    q.add(pod("second", 50));

    let first = q.pop().await;
    // Event arrives while only `first` is in flight.
    q.move_all_to_active_or_backoff(node_added(), None, None);
    let second = q.pop().await;

    // `second` started after the event, so nothing to replay for it.
    q.add_unschedulable(second, vec!["Fit"], vec![]);
    assert_eq!(q.unschedulable_len(), 1, "second must park");

    // `first` overlapped it, so it is requeued.
    q.add_unschedulable(first, vec!["Fit"], vec![]);
    assert_eq!(q.backoff_len(), 1, "first must be requeued");
}

#[tokio::test]
async fn finishing_every_cycle_clears_the_event_timeline() {
    // Otherwise the timeline grows without bound on a busy cluster.
    let q = queue();
    q.add(pod("p", 0));
    let popped = q.pop().await;
    q.move_all_to_active_or_backoff(node_added(), None, None);
    q.done(&popped.uid);

    q.add(pod("later", 0));
    let later = q.pop().await;
    q.add_unschedulable(later, vec!["Fit"], vec![]);

    assert_eq!(
        q.unschedulable_len(),
        1,
        "a stale event must not requeue a pod whose cycle it never overlapped"
    );
}

#[test]
fn events_are_not_recorded_when_no_cycle_is_running() {
    // The common idle case; recording would be pure overhead.
    let q = queue();
    for _ in 0..100 {
        q.move_all_to_active_or_backoff(node_added(), None, None);
    }
    // Nothing to assert directly without exposing internals — but a pod added
    // and rejected afterwards must still park, proving nothing was retained.
    q.add_unschedulable(pod("p", 0), vec!["Fit"], vec![]);
    assert_eq!(q.unschedulable_len(), 1);
}

// ── The safety net ──────────────────────────────────────────────────────

#[test]
fn the_timeout_net_rescues_a_pod_that_has_waited_too_long() {
    let mut hints = HintRegistry::new();
    // A plugin that rejects but subscribes to nothing — precisely the bug the
    // net exists to paper over.
    hints.register("Forgetful", vec![]);
    let q = SchedulingQueue::new(
        hints,
        Arc::new(|a: &PodInfo, b: &PodInfo| a.priority > b.priority),
        Arc::new(|_: &PodInfo| Status::success()),
        BackoffQueue::new(DEFAULT_POD_INITIAL_BACKOFF, DEFAULT_POD_MAX_BACKOFF),
        Duration::from_millis(0), // everything is immediately overdue
    );

    q.add_unschedulable(pod("stranded", 0), vec!["Forgetful"], vec![]);
    q.flush_timed_out();

    assert_eq!(q.unschedulable_len(), 0);
    assert_eq!(q.backoff_len(), 1);
    assert_eq!(
        q.rescued_by_timeout(),
        1,
        "a rescue must be counted — it is a bug report, not routine"
    );
}

#[test]
fn the_timeout_net_does_nothing_when_nothing_has_waited_too_long() {
    // The normal case, and what the counter staying at zero proves.
    let q = queue();
    q.add_unschedulable(pod("p", 0), vec!["Fit"], vec![]);
    q.flush_timed_out();

    assert_eq!(q.unschedulable_len(), 1);
    assert_eq!(q.rescued_by_timeout(), 0);
}

#[test]
fn the_next_timeout_deadline_is_the_oldest_parked_pods() {
    let q = queue();
    assert!(q.next_timeout_deadline().is_none(), "nothing parked, nothing to wake for");

    q.add_unschedulable(pod("p", 0), vec!["Fit"], vec![]);
    assert!(q.next_timeout_deadline().is_some());
}

// ── Removal ─────────────────────────────────────────────────────────────

#[test]
fn deleting_a_pod_removes_it_from_wherever_it_is() {
    let q = queue();
    q.add(pod("active", 0));
    q.add_unschedulable(pod("parked", 0), vec!["Fit"], vec![]);

    q.remove("active");
    q.remove("parked");

    assert_eq!(q.active_len(), 0);
    assert_eq!(q.unschedulable_len(), 0);
}

#[test]
fn activate_forces_a_parked_pod_into_the_active_queue() {
    let q = queue();
    q.add_unschedulable(pod("p", 0), vec!["Fit"], vec![]);

    q.activate(&["p".to_string()]);

    assert_eq!(q.active_len(), 1);
    assert_eq!(q.unschedulable_len(), 0);
}
