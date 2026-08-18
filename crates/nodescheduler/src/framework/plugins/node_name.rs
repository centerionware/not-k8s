//! `NodeName` — honour a pod that named its node directly.
//!
//! # The one plugin whose rejection is genuinely unresolvable
//!
//! `spec.nodeName` is not a preference. A pod that names `worker-3` can never
//! run on `worker-4`, no matter how much capacity is freed there, so every
//! other node is rejected with `UnschedulableAndUnresolvable`.
//!
//! That code is doing real work, not just being descriptive: it excludes those
//! nodes from preemption candidacy. Returning plain `Unschedulable` here would
//! invite the preemptor to evict pods across the entire cluster looking for
//! room that could not possibly help — victims killed for nothing, and the pod
//! still Pending afterwards.
//!
//! In practice a pod with `spec.nodeName` set is normally never seen by a
//! scheduler at all: kubelet picks it up directly and the scheduler's own
//! watch filters it out as already-assigned. This plugin covers the case where
//! one arrives anyway, which is what upstream does too.

use crate::cache::{NodeInfo, PodInfo};
use crate::framework::status::Status;
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::{ClusterEventWithHint, CycleState, FilterPlugin, Plugin};

pub const NAME: &str = "NodeName";

pub struct NodeName;

impl Plugin for NodeName {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        // The requested node may not exist yet. No Node update can rename an
        // existing object, so Add is the sole useful event (v1.33 contract).
        vec![ClusterEventWithHint::always(ClusterEvent::new(
            EventResource::Node,
            ActionType::ADD,
        ))]
    }
}

impl FilterPlugin for NodeName {
    fn filter(&self, _state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status {
        match pod.node_name.as_deref() {
            None => Status::success(),
            Some(wanted) if wanted == node.name => Status::success(),
            Some(_) => Status::unresolvable(NAME, "node(s) didn't match the requested node name"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::plugins::testutil::{node, pod};

    #[test]
    fn a_pod_naming_no_node_is_admitted_anywhere() {
        assert!(NodeName.filter(&CycleState::default(), &pod("p"), &node("any")).is_success());
    }

    #[test]
    fn a_pod_naming_this_node_is_admitted() {
        let mut p = pod("p");
        p.node_name = Some("worker-3".to_string());
        assert!(NodeName
            .filter(&CycleState::default(), &p, &node("worker-3"))
            .is_success());
    }

    #[test]
    fn a_pod_naming_another_node_is_rejected_as_unresolvable() {
        // Unresolvable, not merely unschedulable — see the module header.
        // Getting this wrong sends preemption hunting across the cluster for
        // room that cannot help.
        let mut p = pod("p");
        p.node_name = Some("worker-3".to_string());

        let s = NodeName.filter(&CycleState::default(), &p, &node("worker-4"));
        assert!(!s.is_success());
        assert!(
            !s.code.is_resolvable_by_preemption(),
            "evicting pods elsewhere can never satisfy spec.nodeName"
        );
    }

    #[test]
    fn it_wakes_when_the_requested_node_appears() {
        let events = NodeName.events_to_register();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, ClusterEvent::new(EventResource::Node, ActionType::ADD));
    }
}
