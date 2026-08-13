//! `SchedulingGates` — hold a pod out of the queue entirely until every gate
//! in `spec.schedulingGates` has been removed.
//!
//! # A gated pod is not a rejected pod
//!
//! This is the one place in the crate where a rejection must be *silent* on
//! our part: no `FailedScheduling` event, and no `PodScheduled` condition
//! written by us. The pod has not been rejected by scheduling — it has not
//! entered scheduling. A gate is how an external controller says "not yet",
//! and dashboards must not light up because a queue-management controller is
//! doing its job.
//!
//! The apiserver separately marks the pod `PodScheduled=False` with `reason:
//! SchedulingGated` at admission — that is upstream behaviour, it is what
//! makes `kubectl` show the pod as `SchedulingGated`, and it is not ours to
//! suppress. What we must not do is overwrite that reason with
//! `Unschedulable`, which would turn "waiting on a gate" into "the scheduler
//! could not place this".
//!
//! Writing a condition here is a tempting one-line "improvement" that makes
//! every gated pod look broken to everything watching the cluster. The
//! framework enforces the silence by running PreEnqueue outside the reporting
//! path; this plugin only has to return the right code.
//!
//! # Gates only ever shrink
//!
//! The API forbids adding a gate after creation, so the only transition that
//! matters is the last one being removed. That is why the subscription is the
//! specific `UPDATE_POD_SCHEDULING_GATES_ELIMINATED` bit rather than a general
//! pod update: removing one of three gates changes nothing about whether this
//! pod can be scheduled, and waking every gated pod for it is pure waste on a
//! cluster that uses several.

use crate::cache::PodInfo;
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{ClusterEventWithHint, Plugin, PreEnqueuePlugin};

pub const NAME: &str = "SchedulingGates";

pub struct SchedulingGates;

impl Plugin for SchedulingGates {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        vec![ClusterEventWithHint::always(ClusterEvent::new(
            EventResource::Pod,
            ActionType::UPDATE_POD_SCHEDULING_GATES_ELIMINATED,
        ))]
    }
}

impl PreEnqueuePlugin for SchedulingGates {
    fn pre_enqueue(&self, pod: &PodInfo) -> Status {
        if pod.scheduling_gates.is_empty() {
            return Status::success();
        }
        let names: Vec<&str> = pod.scheduling_gates.iter().map(|g| g.name.as_str()).collect();
        Status::unschedulable(NAME, format!("waiting for scheduling gates: {}", names.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::plugins::testutil::pod;
    use k8s_openapi::api::core::v1::PodSchedulingGate;

    fn gate(name: &str) -> PodSchedulingGate {
        PodSchedulingGate { name: name.to_string() }
    }

    #[test]
    fn an_ungated_pod_is_admitted() {
        assert!(SchedulingGates.pre_enqueue(&pod("p")).is_success());
    }

    #[test]
    fn a_gated_pod_is_held_out_of_the_queue() {
        let mut p = pod("p");
        p.scheduling_gates = vec![gate("example.com/hold")];

        let s = SchedulingGates.pre_enqueue(&p);
        assert!(!s.is_success());
        assert!(s.reasons[0].contains("example.com/hold"));
    }

    #[test]
    fn removing_only_some_gates_still_holds_the_pod() {
        let mut p = pod("p");
        p.scheduling_gates = vec![gate("b")];
        assert!(!SchedulingGates.pre_enqueue(&p).is_success());
    }

    #[test]
    fn the_reason_names_every_gate_being_waited_on() {
        // The only diagnostic a gated pod gets, since it deliberately writes
        // no condition — so it has to name all of them.
        let mut p = pod("p");
        p.scheduling_gates = vec![gate("a"), gate("b")];

        let s = SchedulingGates.pre_enqueue(&p);
        assert!(s.reasons[0].contains('a') && s.reasons[0].contains('b'));
    }

    #[test]
    fn it_subscribes_only_to_the_last_gate_being_removed() {
        // Not a general pod update: removing one of several gates cannot make
        // this pod schedulable, and waking every gated pod for it is waste.
        let events = SchedulingGates.events_to_register();
        let pairs: Vec<(EventResource, ActionType)> =
            events.iter().map(|e| (e.event.resource, e.event.action)).collect();

        assert_eq!(
            pairs,
            vec![(EventResource::Pod, ActionType::UPDATE_POD_SCHEDULING_GATES_ELIMINATED)]
        );
    }

    #[test]
    fn a_pod_label_change_does_not_wake_a_gated_pod() {
        let label_change = ClusterEvent::new(EventResource::Pod, ActionType::UPDATE_POD_LABEL);
        for reg in SchedulingGates.events_to_register() {
            assert!(!reg.event.matches(&label_change));
        }
    }
}
