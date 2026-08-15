//! The binding cycle — everything after a node has been chosen.
//!
//! # Why this is a separate task per pod
//!
//! `PreBind` can block for a long time. `VolumeBinding`'s waits for a
//! provisioner to actually create and bind a PersistentVolume, with a default
//! timeout of **600 seconds**. If that ran on the scheduling loop, one slow
//! storage backend would stop placement for every pod in the cluster, and the
//! symptom would be "the scheduler is hung" rather than "this one pod is
//! waiting on storage".
//!
//! So the scheduling cycle ends at Reserve/Permit, and everything from
//! `PreBind` onward is spawned per pod. Many binding cycles run concurrently;
//! exactly one scheduling cycle runs at a time. The pod's resources are
//! already committed to its node by then (the assume, see `cache/assume.rs`),
//! so the next scheduling cycle sees the capacity as spent even though nothing
//! has been written to the apiserver yet.
//!
//! # Every exit path must release the reservation
//!
//! Because assumed pods never expire, a binding cycle that returns without
//! either finishing or forgetting leaves a node permanently looking fuller
//! than it is. There is no sweeper to catch it. [`bind_one`] therefore treats
//! that pairing the way one treats releasing a lock — every branch below ends
//! in exactly one of `finish_binding` or the failure path's `forget`.
//!
//! # A failed bind frees more than the pod that failed
//!
//! When a binding fails, the resources that pod had assumed become available
//! again. Other pods parked as unschedulable may now fit — but nothing about
//! *their* rejection reasons has changed, so no ordinary event will wake them.
//! The failure path therefore emits an assigned-pod-delete event itself. It is
//! the same rule CLAUDE.md states for `nodelet`: anything that changes real
//! state without going through an object mutation has to re-trigger
//! reconciliation explicitly, because nothing else will.

use crate::cache::PodInfo;
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::{Code, Status};
use crate::framework::{CycleState, Registry};
use crate::queue::SchedulingQueue;
use std::sync::{Arc, Mutex};

/// What the binding cycle concluded, for the caller's bookkeeping.
#[derive(Debug, PartialEq, Eq)]
pub enum BindOutcome {
    Bound,
    /// Rejected in a way that means "try again later" rather than "this is
    /// broken" — the pod goes back through the normal unschedulable path.
    Rejected { reason: String, plugins: Vec<&'static str> },
    /// Something failed. The pod is requeued and the capacity released.
    Failed { reason: String },
}

/// Decide how a binding-cycle failure should be handled.
///
/// Split out as a pure function because the routing rule is easy to get
/// subtly wrong and impossible to observe once it is buried in an async
/// path: an `Unschedulable`/`Pending` verdict is a scheduling decision and
/// belongs back in the queue's unschedulable set with its plugins recorded,
/// while an `Error` is a malfunction and must not be recorded as though some
/// plugin's registered events could fix it.
pub fn classify_failure(status: &Status) -> BindOutcome {
    match status.code {
        Code::Unschedulable | Code::UnschedulableAndUnresolvable | Code::Pending => {
            BindOutcome::Rejected {
                reason: status.to_string(),
                plugins: if status.plugin.is_empty() {
                    Vec::new()
                } else {
                    vec![status.plugin]
                },
            }
        }
        _ => BindOutcome::Failed { reason: status.to_string() },
    }
}

/// Run the binding cycle for one pod.
///
/// `state` carries the reservations made during the scheduling cycle, so it
/// moves in here rather than being shared — a second cycle must never be able
/// to observe or unwind this pod's Reserve state.
pub async fn bind_one(
    registry: &Registry,
    mut state: CycleState,
    pod: Arc<PodInfo>,
    node: String,
) -> BindOutcome {
    // ── Permit ──────────────────────────────────────────────────────────
    // Waiting on a permit happens here, in the binding cycle, even though the
    // Permit plugins themselves ran in the scheduling cycle. No default
    // plugin uses it; a gang-scheduling plugin would.

    // ── PreBind ─────────────────────────────────────────────────────────
    for plugin in &registry.pre_bind {
        let status = plugin.pre_bind(&pod, &node).await;
        if !status.is_success() {
            crate::cycle::run_unreserve(registry, &mut state, &pod, &node);
            return classify_failure(&status);
        }
    }

    // ── Bind ────────────────────────────────────────────────────────────
    // The first plugin that does not decline handles it; the rest are skipped.
    let mut bound = false;
    for plugin in &registry.bind {
        let status = plugin.bind(&pod, &node).await;
        if status.is_skip() {
            continue;
        }
        if !status.is_success() {
            crate::cycle::run_unreserve(registry, &mut state, &pod, &node);
            return classify_failure(&status);
        }
        bound = true;
        break;
    }

    if !bound {
        // Every Bind plugin declined. A profile with no usable binder cannot
        // place anything, so this is a configuration error rather than a
        // scheduling outcome.
        crate::cycle::run_unreserve(registry, &mut state, &pod, &node);
        return BindOutcome::Failed {
            reason: format!(
                "no Bind plugin accepted pod {} for node {node} — the profile has no usable binder",
                pod.key()
            ),
        };
    }

    // ── PostBind ────────────────────────────────────────────────────────
    // Informational only. A failure here cannot un-bind the pod, so nothing
    // is checked and nothing is unwound.
    for plugin in &registry.post_bind {
        plugin.post_bind(&pod, &node).await;
    }

    BindOutcome::Bound
}

/// Apply a binding outcome to the queue and the assume set.
///
/// Kept next to `bind_one` rather than inside it so the decision and its
/// consequences can be tested apart — and so there is exactly one place that
/// releases a reservation.
pub fn handle_outcome(
    outcome: &BindOutcome,
    pod: Arc<PodInfo>,
    queue: &SchedulingQueue,
    assumed: &Mutex<crate::cache::AssumedPods>,
    cache: &Mutex<crate::cache::Cache>,
) {
    match outcome {
        BindOutcome::Bound => {
            // The reservation stays until the informer delivers the real bound
            // pod — see cache/assume.rs for why releasing here would be wrong.
            assumed.lock().unwrap().finish_binding(&pod.uid);
        }
        BindOutcome::Rejected { reason, plugins } => {
            tracing::info!(pod = %pod.key(), %reason, "binding cycle rejected the pod");
            release(&pod, assumed, cache);
            queue.add_unschedulable(pod, plugins.clone(), Vec::new());
        }
        BindOutcome::Failed { reason } => {
            tracing::warn!(pod = %pod.key(), %reason, "binding cycle failed; requeueing");
            release(&pod, assumed, cache);
            // No plugin to name, so straight to backoff — see
            // requeue_after_failure's own doc comment for why
            // add_unschedulable's event-hint matching would strand this pod.
            queue.requeue_after_failure(pod);

            // The capacity this pod had assumed is free again. Nothing about
            // any other pod's rejection changed, so no ordinary event will
            // wake them — this has to be emitted explicitly.
            queue.move_all_to_active_or_backoff(
                ClusterEvent::new(EventResource::AssignedPod, ActionType::DELETE),
                None,
                None,
            );
        }
    }
}

/// Undo the assume: drop the reservation **and** un-commit the resources.
///
/// Both halves, always. The scheduling cycle does two things when it places a
/// pod — records the reservation in `AssumedPods` and adds the pod to the
/// node's running totals in the `Cache` — and forgetting only the first leaves
/// the node permanently looking fuller than it is. There is no sweeper to
/// notice: assumed pods never expire, and the `Cache` is only corrected by an
/// informer event that will never arrive for a pod that was never bound.
///
/// The visible symptom would be a node that quietly stops accepting pods it
/// has room for, with nothing in any log to say why, and it would accumulate
/// with every failed bind until the process restarted.
fn release(
    pod: &PodInfo,
    assumed: &Mutex<crate::cache::AssumedPods>,
    cache: &Mutex<crate::cache::Cache>,
) {
    assumed.lock().unwrap().forget(&pod.uid);
    cache.lock().unwrap().remove_pod(&pod.uid);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unschedulable_verdict_is_a_scheduling_decision_not_a_malfunction() {
        let out = classify_failure(&Status::unschedulable("VolumeBinding", "no PV available"));
        match out {
            BindOutcome::Rejected { plugins, .. } => {
                assert_eq!(plugins, vec!["VolumeBinding"]);
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn a_pending_verdict_is_also_a_scheduling_decision() {
        let out = classify_failure(&Status::pending("DynamicResources", "claim allocating"));
        assert!(matches!(out, BindOutcome::Rejected { .. }));
    }

    #[test]
    fn an_unresolvable_verdict_is_still_routed_back_to_the_queue() {
        let out = classify_failure(&Status::unresolvable("VolumeZone", "wrong zone"));
        assert!(matches!(out, BindOutcome::Rejected { .. }));
    }

    #[test]
    fn an_error_is_a_malfunction_and_records_no_plugin_to_wake_on() {
        // Recording a plugin here would tell the queue that plugin's
        // registered events could fix an apiserver outage, which they cannot —
        // the pod would then wait for an event that is irrelevant to it.
        let out = classify_failure(&Status::error("DefaultBinder", "connection refused"));
        match out {
            BindOutcome::Failed { reason } => assert!(reason.contains("connection refused")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_rejection_with_no_named_plugin_records_none_rather_than_an_empty_name() {
        // An empty string in the plugin list would be looked up in the hint
        // registry, miss, and silently contribute nothing — better to not be
        // there at all.
        let out = classify_failure(&Status {
            code: Code::Unschedulable,
            plugin: "",
            reasons: vec!["something".to_string()],
        });
        match out {
            BindOutcome::Rejected { plugins, .. } => assert!(plugins.is_empty()),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }
}
