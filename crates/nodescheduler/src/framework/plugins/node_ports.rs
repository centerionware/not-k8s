//! `NodePorts` — don't put two pods on the same host port.
//!
//! A `hostPort` bypasses the Service layer and binds directly on the node's
//! network namespace, so two pods claiming the same one cannot coexist. The
//! collision rule is the fiddly part and lives in
//! [`crate::cache::HostPort::conflicts_with`]: same protocol and port, and
//! either address is a wildcard or they are equal.
//!
//! # Why PreFilter exists for something this small
//!
//! Walking a pod's containers to collect its host ports is cheap. Doing it
//! once per *node* rather than once per pod is not: on a 5000-node cluster
//! that is 5000 repetitions of identical work per scheduling cycle, and the
//! cycle runs once per pending pod. PreFilter computes it once into
//! `CycleState` and Filter reads it. Every PreFilter in this crate exists for
//! that reason.
//!
//! It also returns `Skip` for the overwhelmingly common case of a pod with no
//! host ports at all, which switches this plugin's Filter off entirely for the
//! cycle.

use crate::cache::{HostPort, NodeInfo, PodInfo};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{
    ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreFilterPlugin,
};

pub const NAME: &str = "NodePorts";

/// The pod's host-port claims, computed once per cycle.
struct WantedPorts(Vec<HostPort>);

pub struct NodePorts;

impl Plugin for NodePorts {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        // The only thing that frees a host port is the pod holding it going
        // away. A pod being *added* cannot help, so this subscribes to Delete
        // alone rather than to pod changes generally — on a busy cluster that
        // is the difference between waking port-blocked pods on every pod
        // creation and waking them only when a port actually opens.
        vec![
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::AssignedPod,
                ActionType::DELETE,
            )),
            // A new node has no ports used at all.
            ClusterEventWithHint::always(ClusterEvent::new(
                EventResource::Node,
                ActionType::ADD,
            )),
        ]
    }
}

impl PreFilterPlugin for NodePorts {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        _snapshot: &crate::cache::Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        if pod.host_ports.is_empty() {
            state.skip_filter(NAME);
            return (Status::skip(), None);
        }
        state.write(NAME, WantedPorts(pod.host_ports.clone()));
        (Status::success(), None)
    }
}

impl FilterPlugin for NodePorts {
    fn filter(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status {
        // Fall back to the pod's own list when PreFilter did not run — the
        // preemption dry-run path constructs states directly, and a Filter
        // that silently passed everything there would let preemption believe
        // a port conflict was resolvable.
        let wanted: &[HostPort] = match state.read::<WantedPorts>(NAME) {
            Some(w) => &w.0,
            None => &pod.host_ports,
        };
        if wanted.is_empty() {
            return Status::success();
        }
        for want in wanted {
            if node.port_conflicts(want) {
                return Status::unschedulable(NAME, "node(s) didn't have free ports for the requested pod ports");
            }
        }
        Status::success()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Snapshot;
    use crate::framework::plugins::testutil::{node, pod};

    fn port(ip: &str, p: i32) -> HostPort {
        HostPort { protocol: "TCP".to_string(), ip: ip.to_string(), port: p }
    }

    #[test]
    fn a_pod_wanting_no_host_ports_skips_the_plugin_entirely() {
        // The common case; it must cost nothing per node.
        let mut state = CycleState::default();
        let (status, _) = NodePorts.pre_filter(&mut state, &pod("p"), &Snapshot::default());

        assert!(status.is_skip());
        assert!(state.filter_skipped(NAME));
    }

    #[test]
    fn a_free_port_is_admitted() {
        let mut p = pod("web");
        p.host_ports = vec![port("", 80)];
        let mut state = CycleState::default();
        NodePorts.pre_filter(&mut state, &p, &Snapshot::default());

        assert!(NodePorts.filter(&state, &p, &node("n")).is_success());
    }

    #[test]
    fn a_taken_port_is_rejected() {
        let mut p = pod("web");
        p.host_ports = vec![port("", 80)];
        let mut n = node("n");
        n.used_ports = vec![port("", 80)];
        let mut state = CycleState::default();
        NodePorts.pre_filter(&mut state, &p, &Snapshot::default());

        let s = NodePorts.filter(&state, &p, &n);
        assert!(!s.is_success());
        assert!(s.reasons[0].contains("free ports"));
    }

    #[test]
    fn a_port_rejection_is_resolvable_by_preemption() {
        // Evicting the pod holding the port genuinely frees it.
        let mut p = pod("web");
        p.host_ports = vec![port("", 80)];
        let mut n = node("n");
        n.used_ports = vec![port("", 80)];
        let mut state = CycleState::default();
        NodePorts.pre_filter(&mut state, &p, &Snapshot::default());

        assert!(NodePorts.filter(&state, &p, &n).code.is_resolvable_by_preemption());
    }

    #[test]
    fn a_wildcard_claim_collides_with_a_specific_address() {
        let mut p = pod("web");
        p.host_ports = vec![port("0.0.0.0", 80)];
        let mut n = node("n");
        n.used_ports = vec![port("10.0.0.1", 80)];
        let mut state = CycleState::default();
        NodePorts.pre_filter(&mut state, &p, &Snapshot::default());

        assert!(!NodePorts.filter(&state, &p, &n).is_success());
    }

    #[test]
    fn the_same_port_on_different_addresses_is_fine() {
        let mut p = pod("web");
        p.host_ports = vec![port("10.0.0.2", 80)];
        let mut n = node("n");
        n.used_ports = vec![port("10.0.0.1", 80)];
        let mut state = CycleState::default();
        NodePorts.pre_filter(&mut state, &p, &Snapshot::default());

        assert!(NodePorts.filter(&state, &p, &n).is_success());
    }

    #[test]
    fn filtering_without_prefilter_state_still_checks_the_pods_ports() {
        // Preemption's dry runs construct states directly. Passing everything
        // here would let preemption think a port conflict was resolvable.
        let mut p = pod("web");
        p.host_ports = vec![port("", 80)];
        let mut n = node("n");
        n.used_ports = vec![port("", 80)];

        assert!(!NodePorts.filter(&CycleState::default(), &p, &n).is_success());
    }

    #[test]
    fn it_wakes_on_a_pod_deletion_but_not_a_pod_creation() {
        // Only a departing pod can free a port.
        let events = NodePorts.events_to_register();
        let deleted = ClusterEvent::new(EventResource::AssignedPod, ActionType::DELETE);
        let added = ClusterEvent::new(EventResource::AssignedPod, ActionType::ADD);

        assert!(events.iter().any(|e| e.event.matches(&deleted)));
        assert!(!events.iter().any(|e| e.event.matches(&added)));
    }
}
