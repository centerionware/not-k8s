//! Tests for PodTopologySpread.
//!
//! The skew arithmetic is the whole plugin, and every way of getting it wrong
//! is quiet: an off-by-one in either direction either packs a workload into
//! one zone or refuses to place it anywhere, and both look like capacity
//! problems rather than a scheduling bug.

use super::*;
use crate::cache::Cache;
use crate::framework::plugins::testutil::pod;
use k8s_openapi::api::core::v1::{Node, NodeStatus, Taint};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use std::collections::BTreeMap;
use std::sync::Arc;

const ZONE: &str = "topology.kubernetes.io/zone";

fn api_node(name: &str, zone: &str) -> Node {
    Node {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(BTreeMap::from([
                (ZONE.to_string(), zone.to_string()),
                ("kubernetes.io/hostname".to_string(), name.to_string()),
            ])),
            ..Default::default()
        },
        status: Some(NodeStatus {
            allocatable: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity("4".to_string())),
                ("pods".to_string(), Quantity("110".to_string())),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn placed(uid: &str, node: &str) -> Arc<PodInfo> {
    Arc::new(PodInfo {
        namespace: "default".to_string(),
        name: uid.to_string(),
        uid: uid.to_string(),
        node_name: Some(node.to_string()),
        labels: BTreeMap::from([("app".to_string(), "web".to_string())]),
        ..Default::default()
    })
}

fn constraint(max_skew: i32, hard: bool) -> TopologySpreadConstraint {
    TopologySpreadConstraint {
        max_skew,
        topology_key: ZONE.to_string(),
        when_unsatisfiable: if hard { "DoNotSchedule" } else { "ScheduleAnyway" }.to_string(),
        label_selector: Some(LabelSelector {
            match_labels: Some(BTreeMap::from([("app".to_string(), "web".to_string())])),
            match_expressions: None,
        }),
        ..Default::default()
    }
}

fn incoming(c: TopologySpreadConstraint) -> PodInfo {
    let mut p = pod("incoming");
    p.namespace = "default".to_string();
    p.labels = BTreeMap::from([("app".to_string(), "web".to_string())]);
    p.topology_spread_constraints = vec![c];
    p
}

/// Three zones, one node each, with the given pods placed.
fn cluster(placements: &[(&str, &str)]) -> Snapshot {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n-east", "east"));
    cache.upsert_node(&api_node("n-west", "west"));
    cache.upsert_node(&api_node("n-north", "north"));
    for (uid, node) in placements {
        cache.add_pod(placed(uid, node));
    }
    cache.snapshot()
}

fn prefiltered(p: &PodInfo, snap: &Snapshot) -> (PodTopologySpread, CycleState, Status) {
    let plugin = PodTopologySpread::default();
    let mut state = CycleState::default();
    let (status, _) = plugin.pre_filter(&mut state, p, snap);
    (plugin, state, status)
}

fn node<'a>(snap: &'a Snapshot, name: &str) -> &'a NodeInfo {
    snap.node(name).expect("node in snapshot")
}

// ── Skew ────────────────────────────────────────────────────────────────

#[test]
fn an_empty_cluster_accepts_the_first_pod_anywhere() {
    let snap = cluster(&[]);
    let p = incoming(constraint(1, true));
    let (plugin, state, _) = prefiltered(&p, &snap);

    for n in ["n-east", "n-west", "n-north"] {
        assert!(plugin.filter(&state, &p, node(&snap, n)).is_success(), "{n}");
    }
}

#[test]
fn a_second_pod_avoids_the_occupied_zone_at_max_skew_one() {
    // east has 1, the others 0, so globalMin is 0. Placing in east would make
    // skew 2-0=2 > 1; placing elsewhere makes 1-0=1, which is allowed.
    let snap = cluster(&[("a", "n-east")]);
    let p = incoming(constraint(1, true));
    let (plugin, state, _) = prefiltered(&p, &snap);

    assert!(!plugin.filter(&state, &p, node(&snap, "n-east")).is_success());
    assert!(plugin.filter(&state, &p, node(&snap, "n-west")).is_success());
    assert!(plugin.filter(&state, &p, node(&snap, "n-north")).is_success());
}

#[test]
fn a_wider_max_skew_tolerates_more_imbalance() {
    let snap = cluster(&[("a", "n-east")]);
    let p = incoming(constraint(2, true));
    let (plugin, state, _) = prefiltered(&p, &snap);

    assert!(
        plugin.filter(&state, &p, node(&snap, "n-east")).is_success(),
        "skew of 2 is allowed when maxSkew is 2"
    );
}

#[test]
fn an_evenly_filled_cluster_accepts_a_pod_in_any_zone() {
    let snap = cluster(&[("a", "n-east"), ("b", "n-west"), ("c", "n-north")]);
    let p = incoming(constraint(1, true));
    let (plugin, state, _) = prefiltered(&p, &snap);

    // All at 1, globalMin 1, so any placement gives skew 2-1=1.
    for n in ["n-east", "n-west", "n-north"] {
        assert!(plugin.filter(&state, &p, node(&snap, n)).is_success(), "{n}");
    }
}

#[test]
fn only_pods_matching_the_selector_are_counted() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n-east", "east"));
    cache.upsert_node(&api_node("n-west", "west"));
    cache.upsert_node(&api_node("n-north", "north"));
    let mut unrelated = (*placed("other", "n-east")).clone();
    unrelated.labels = BTreeMap::from([("app".to_string(), "batch".to_string())]);
    cache.add_pod(Arc::new(unrelated));
    let snap = cache.snapshot();

    let p = incoming(constraint(1, true));
    let (plugin, state, _) = prefiltered(&p, &snap);

    assert!(
        plugin.filter(&state, &p, node(&snap, "n-east")).is_success(),
        "a pod with different labels must not count against this constraint"
    );
}

#[test]
fn pods_in_another_namespace_are_not_counted() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n-east", "east"));
    cache.upsert_node(&api_node("n-west", "west"));
    cache.upsert_node(&api_node("n-north", "north"));
    let mut elsewhere = (*placed("other", "n-east")).clone();
    elsewhere.namespace = "kube-system".to_string();
    cache.add_pod(Arc::new(elsewhere));
    let snap = cache.snapshot();

    let p = incoming(constraint(1, true));
    let (plugin, state, _) = prefiltered(&p, &snap);

    assert!(plugin.filter(&state, &p, node(&snap, "n-east")).is_success());
}

// ── minDomains ──────────────────────────────────────────────────────────

#[test]
fn min_domains_keeps_a_constraint_biting_on_a_narrow_cluster() {
    // One zone only: globalMin would be that zone's own count, so skew is
    // always 0 and the constraint silently does nothing. minDomains forces
    // globalMin to 0 until the cluster is wide enough.
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n-east", "east"));
    cache.add_pod(placed("a", "n-east"));
    let snap = cache.snapshot();

    let mut c = constraint(1, true);
    c.min_domains = Some(3);
    let p = incoming(c);
    let (plugin, state, _) = prefiltered(&p, &snap);

    assert!(
        !plugin.filter(&state, &p, node(&snap, "n-east")).is_success(),
        "with fewer than minDomains domains, globalMin is 0 and the skew limit applies"
    );
}

#[test]
fn without_min_domains_a_single_domain_never_blocks() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n-east", "east"));
    cache.add_pod(placed("a", "n-east"));
    let snap = cache.snapshot();

    let p = incoming(constraint(1, true));
    let (plugin, state, _) = prefiltered(&p, &snap);

    // globalMin is 1 (the only domain), so skew is 2-1 = 1, within maxSkew.
    assert!(plugin.filter(&state, &p, node(&snap, "n-east")).is_success());
}

// ── Eligibility ─────────────────────────────────────────────────────────

#[test]
fn a_zone_the_pod_could_never_use_does_not_drag_globalmin_down() {
    // nodeAffinityPolicy defaults to Honor precisely for this. If the
    // unreachable zone counted, globalMin would be 0 forever and the pod
    // could never be placed in the zone it *is* allowed to use.
    let snap = cluster(&[("a", "n-east")]);
    let mut p = incoming(constraint(1, true));
    p.node_selector = BTreeMap::from([(ZONE.to_string(), "east".to_string())]);
    let (plugin, state, _) = prefiltered(&p, &snap);

    assert!(
        plugin.filter(&state, &p, node(&snap, "n-east")).is_success(),
        "east is the only eligible domain, so its own count is globalMin and skew is 1"
    );
}

#[test]
fn ignoring_node_affinity_lets_an_unreachable_zone_count() {
    let snap = cluster(&[("a", "n-east")]);
    let mut c = constraint(1, true);
    c.node_affinity_policy = Some("Ignore".to_string());
    let mut p = incoming(c);
    p.node_selector = BTreeMap::from([(ZONE.to_string(), "east".to_string())]);
    let (plugin, state, _) = prefiltered(&p, &snap);

    assert!(
        !plugin.filter(&state, &p, node(&snap, "n-east")).is_success(),
        "with Ignore the empty zones count, globalMin is 0, and east is at its limit"
    );
}

#[test]
fn taints_are_ignored_by_default_and_honoured_on_request() {
    let mut cache = Cache::new();
    let mut tainted = api_node("n-west", "west");
    tainted.spec = Some(k8s_openapi::api::core::v1::NodeSpec {
        taints: Some(vec![Taint {
            key: "dedicated".to_string(),
            value: Some("gpu".to_string()),
            effect: "NoSchedule".to_string(),
            ..Default::default()
        }]),
        ..Default::default()
    });
    cache.upsert_node(&api_node("n-east", "east"));
    cache.upsert_node(&tainted);
    cache.add_pod(placed("a", "n-east"));
    let snap = cache.snapshot();

    // Default (Ignore): west counts as an empty domain, so globalMin is 0 and
    // east is at its skew limit.
    let p = incoming(constraint(1, true));
    let (plugin, state, _) = prefiltered(&p, &snap);
    assert!(!plugin.filter(&state, &p, node(&snap, "n-east")).is_success());

    // Honor: west is ineligible, east is the only domain, so it is allowed.
    let mut c = constraint(1, true);
    c.node_taints_policy = Some("Honor".to_string());
    let p2 = incoming(c);
    let (plugin2, state2, _) = prefiltered(&p2, &snap);
    assert!(plugin2.filter(&state2, &p2, node(&snap, "n-east")).is_success());
}

#[test]
fn a_node_without_the_topology_label_is_rejected_by_a_hard_constraint() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n-east", "east"));
    let mut bare = api_node("bare", "unused");
    bare.metadata.labels = Some(BTreeMap::from([(
        "kubernetes.io/hostname".to_string(),
        "bare".to_string(),
    )]));
    cache.upsert_node(&bare);
    let snap = cache.snapshot();

    let p = incoming(constraint(1, true));
    let (plugin, state, _) = prefiltered(&p, &snap);

    let status = plugin.filter(&state, &p, node(&snap, "bare"));
    assert!(!status.is_success());
    // Matches upstream exactly: eviction cannot add a label to a node, so a
    // node missing the topology key is never a preemption candidate.
    assert!(!status.code.is_resolvable_by_preemption());
}

#[test]
fn a_hard_constraint_with_no_eligible_domain_is_unresolvable() {
    // Nothing can be evicted to create a domain, so preemption must not be
    // invited to try.
    let mut cache = Cache::new();
    let mut bare = api_node("bare", "unused");
    bare.metadata.labels = Some(BTreeMap::from([(
        "kubernetes.io/hostname".to_string(),
        "bare".to_string(),
    )]));
    cache.upsert_node(&bare);
    let snap = cache.snapshot();

    let p = incoming(constraint(1, true));
    let (_, _, status) = prefiltered(&p, &snap);

    assert!(!status.is_success());
    assert!(
        !status.code.is_resolvable_by_preemption(),
        "no eviction can create a topology domain"
    );
}

// ── ScheduleAnyway ──────────────────────────────────────────────────────

#[test]
fn a_soft_constraint_never_rejects_a_node() {
    let snap = cluster(&[("a", "n-east"), ("b", "n-east"), ("c", "n-east")]);
    let p = incoming(constraint(1, false));
    let (plugin, state, _) = prefiltered(&p, &snap);

    assert!(
        plugin.filter(&state, &p, node(&snap, "n-east")).is_success(),
        "ScheduleAnyway is a preference; turning it into a rejection is an outage"
    );
}

#[test]
fn a_soft_constraint_prefers_the_emptier_domain() {
    let snap = cluster(&[("a", "n-east"), ("b", "n-east")]);
    let p = incoming(constraint(1, false));
    let (plugin, state, _) = prefiltered(&p, &snap);

    let mut scores = [
        plugin.score(&state, &p, node(&snap, "n-east")).unwrap(),
        plugin.score(&state, &p, node(&snap, "n-west")).unwrap(),
    ];
    assert!(scores[0] > scores[1], "raw score counts occupancy, so east is higher before inversion");

    plugin.normalize(&state, &p, &mut scores);
    assert!(
        scores[1] > scores[0],
        "after inversion the emptier zone must score higher, got east={} west={}",
        scores[0],
        scores[1]
    );
}

#[test]
fn a_hard_constraint_contributes_nothing_to_the_score() {
    let snap = cluster(&[("a", "n-east")]);
    let p = incoming(constraint(1, true));
    let (plugin, state, _) = prefiltered(&p, &snap);

    assert_eq!(plugin.score(&state, &p, node(&snap, "n-west")).unwrap(), 0);
}

#[test]
fn larger_topologies_carry_more_weight() {
    assert!(topology_normalizing_weight(3) > topology_normalizing_weight(1));
}

#[test]
fn each_additional_domain_is_worth_less_than_the_last() {
    // The saturation property, stated as *marginal* weight — one more domain
    // at 300 versus one more at 3.
    //
    // The first version of this test compared w(400)-w(300) against
    // w(4)-w(3) and failed, correctly: that is a hundred-domain span against
    // a one-domain span, so of course it is bigger. Unequal intervals say the
    // opposite of the property and sound just as reasonable.
    let step_at_3 = topology_normalizing_weight(4) - topology_normalizing_weight(3);
    let step_at_300 = topology_normalizing_weight(301) - topology_normalizing_weight(300);

    assert!(
        step_at_300 < step_at_3,
        "one more domain should matter less at 300 than at 3, got {step_at_300} vs {step_at_3}"
    );
}

// ── matchLabelKeys ──────────────────────────────────────────────────────

#[test]
fn match_label_keys_narrows_the_selector_to_this_pods_own_revision() {
    // How a Deployment rollout spreads each revision independently instead of
    // counting the previous revision's pods as its own.
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n-east", "east"));
    cache.upsert_node(&api_node("n-west", "west"));
    let mut old_revision = (*placed("old", "n-east")).clone();
    old_revision
        .labels
        .insert("pod-template-hash".to_string(), "aaa".to_string());
    cache.add_pod(Arc::new(old_revision));
    let snap = cache.snapshot();

    let mut c = constraint(1, true);
    c.match_label_keys = Some(vec!["pod-template-hash".to_string()]);
    let mut p = incoming(c);
    p.labels.insert("pod-template-hash".to_string(), "bbb".to_string());

    let (plugin, state, _) = prefiltered(&p, &snap);

    assert!(
        plugin.filter(&state, &p, node(&snap, "n-east")).is_success(),
        "the previous revision's pod must not count against the new revision's spread"
    );
}

#[test]
fn a_match_label_key_the_pod_does_not_carry_is_skipped() {
    let snap = cluster(&[("a", "n-east")]);
    let mut c = constraint(1, true);
    c.match_label_keys = Some(vec!["absent".to_string()]);
    let p = incoming(c);
    let (plugin, state, _) = prefiltered(&p, &snap);

    // Behaves as though the key were not listed at all.
    assert!(!plugin.filter(&state, &p, node(&snap, "n-east")).is_success());
}

// ── Preemption ──────────────────────────────────────────────────────────

#[test]
fn hypothetically_removing_a_pod_relieves_the_skew() {
    let snap = cluster(&[("a", "n-east")]);
    let p = incoming(constraint(1, true));
    let (plugin, mut state, _) = prefiltered(&p, &snap);

    assert!(!plugin.filter(&state, &p, node(&snap, "n-east")).is_success());

    plugin.remove_pod(&mut state, &p, &placed("a", "n-east"), node(&snap, "n-east"));

    assert!(plugin.filter(&state, &p, node(&snap, "n-east")).is_success());
}

#[test]
fn hypothetically_adding_a_pod_creates_skew() {
    let snap = cluster(&[]);
    let p = incoming(constraint(1, true));
    let (plugin, mut state, _) = prefiltered(&p, &snap);

    assert!(plugin.filter(&state, &p, node(&snap, "n-east")).is_success());

    plugin.add_pod(&mut state, &p, &placed("x", "n-east"), node(&snap, "n-east"));

    assert!(!plugin.filter(&state, &p, node(&snap, "n-east")).is_success());
}

// ── Cost and events ─────────────────────────────────────────────────────

#[test]
fn a_pod_with_no_constraints_skips_the_plugin() {
    let snap = cluster(&[("a", "n-east")]);
    let mut p = pod("plain");
    p.namespace = "default".to_string();
    let plugin = PodTopologySpread::default();
    let mut state = CycleState::default();

    let (status, _) = plugin.pre_filter(&mut state, &p, &snap);

    assert!(status.is_skip());
    assert!(state.filter_skipped(NAME));
    assert!(state.score_skipped(NAME));
}

#[test]
fn it_registers_exactly_the_events_that_can_unstick_a_rejected_pod() {
    let events = PodTopologySpread::default().events_to_register();
    let pairs: Vec<(EventResource, ActionType)> =
        events.iter().map(|e| (e.event.resource, e.event.action)).collect();

    assert_eq!(
        pairs,
        vec![
            (
                EventResource::AssignedPod,
                ActionType::ADD | ActionType::DELETE | ActionType::UPDATE_POD_LABEL
            ),
            (
                EventResource::Node,
                ActionType::ADD
                    | ActionType::DELETE
                    | ActionType::UPDATE_NODE_LABEL
                    | ActionType::UPDATE_NODE_TAINT
            ),
        ]
    );
}

#[test]
fn a_node_heartbeat_does_not_wake_a_pod_this_plugin_rejected() {
    let heartbeat = ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_CONDITION);
    for reg in PodTopologySpread::default().events_to_register() {
        assert!(!reg.event.matches(&heartbeat));
    }
}

// ── System default constraints ──────────────────────────────────────────
//
// The failure mode these guard is silent in both directions: applying the
// defaults to a pod no workload selects spreads it against strangers, and
// failing to apply them to a Deployment's pods packs a replica set into one
// zone while every counter still reads zero.

fn workload(ns: &str, app: &str) -> crate::cache::WorkloadSelector {
    crate::cache::WorkloadSelector {
        namespace: ns.to_string(),
        selector: LabelSelector {
            match_labels: Some(BTreeMap::from([("app".to_string(), app.to_string())])),
            match_expressions: None,
        },
    }
}

/// The same three zones, plus workloads that select `app=web`.
fn cluster_with_workloads(
    placements: &[(&str, &str)],
    workloads: &[(&str, crate::cache::WorkloadSelector)],
) -> Snapshot {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n-east", "east"));
    cache.upsert_node(&api_node("n-west", "west"));
    cache.upsert_node(&api_node("n-north", "north"));
    for (uid, node) in placements {
        cache.add_pod(placed(uid, node));
    }
    for (key, w) in workloads {
        cache.upsert_workload(key.to_string(), w.clone());
    }
    cache.snapshot()
}

fn bare(name: &str) -> PodInfo {
    let mut p = pod(name);
    p.namespace = "default".to_string();
    p.labels = BTreeMap::from([("app".to_string(), "web".to_string())]);
    p
}

#[test]
fn a_pod_no_workload_selects_gets_no_default_constraints() {
    // Upstream's `selector.Empty()` check. Defaulting to a constraint that
    // matches everything would spread a standalone pod against unrelated
    // workloads — the opposite of what an absent selector means.
    let snap = cluster_with_workloads(&[("a", "n-east")], &[]);
    let (_, state, status) = prefiltered(&bare("incoming"), &snap);
    assert!(status.is_skip(), "no selecting workload means no defaults: {status:?}");
    assert!(state.score_skipped(NAME));
}

#[test]
fn a_pod_selected_by_a_workload_gets_the_two_system_defaults() {
    let snap = cluster_with_workloads(&[("a", "n-east")], &[("rs/default/web", workload("default", "web"))]);
    let (_, state, status) = prefiltered(&bare("incoming"), &snap);
    assert!(status.is_success(), "{status:?}");

    let s = state.read::<SpreadState>(NAME).expect("state written");
    let mut keys: Vec<&str> = s.constraints.iter().map(|c| c.topology_key.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["kubernetes.io/hostname", "topology.kubernetes.io/zone"]);

    let zone = s.constraints.iter().find(|c| c.topology_key == ZONE).unwrap();
    let host =
        s.constraints.iter().find(|c| c.topology_key == "kubernetes.io/hostname").unwrap();
    assert_eq!((zone.max_skew, host.max_skew), (3, 5));
}

#[test]
fn the_system_defaults_never_make_a_node_infeasible() {
    // Both are ScheduleAnyway, which is why upstream's PreFilter — which asks
    // for DoNotSchedule constraints only — always gets an empty list from the
    // defaulting path. Resolving them as soft here is the same guarantee: no
    // pod is refused that upstream would have placed.
    let snap = cluster_with_workloads(
        // Enough in one zone to blow past maxSkew 3 if it were ever hard.
        &[("a", "n-east"), ("b", "n-east"), ("c", "n-east"), ("d", "n-east"), ("e", "n-east")],
        &[("rs/default/web", workload("default", "web"))],
    );
    let (plugin, state, status) = prefiltered(&bare("incoming"), &snap);
    assert!(status.is_success(), "{status:?}");
    assert!(
        state.read::<SpreadState>(NAME).unwrap().constraints.iter().all(|c| !c.hard),
        "system defaults are ScheduleAnyway"
    );
    let p = bare("incoming");
    for name in ["n-east", "n-west", "n-north"] {
        assert!(
            plugin.filter(&state, &p, node(&snap, name)).is_success(),
            "{name} must stay feasible under the defaults"
        );
    }
    // But they must still move the score, or the feature does nothing.
    let east = plugin.score(&state, &p, node(&snap, "n-east")).unwrap();
    let west = plugin.score(&state, &p, node(&snap, "n-west")).unwrap();
    assert!(east > west, "the crowded zone must score worse pre-normalization: {east} vs {west}");
}

#[test]
fn a_pods_own_constraints_replace_the_defaults_rather_than_adding_to_them() {
    let snap = cluster_with_workloads(&[], &[("rs/default/web", workload("default", "web"))]);
    let (_, state, _) = prefiltered(&incoming(constraint(1, true)), &snap);
    let s = state.read::<SpreadState>(NAME).unwrap();
    assert_eq!(s.constraints.len(), 1, "defaults must not be merged in");
    assert!(s.constraints[0].hard);
}

#[test]
fn several_workloads_selecting_the_same_pod_are_anded() {
    // A pod behind a Service *and* owned by a ReplicaSet spreads against the
    // intersection. Taking either alone counts pods that are not its peers.
    let mut svc = workload("default", "web");
    svc.selector.match_labels =
        Some(BTreeMap::from([("tier".to_string(), "front".to_string())]));
    let snap = cluster_with_workloads(
        &[],
        &[("rs/default/web", workload("default", "web")), ("svc/default/web", svc)],
    );
    let mut p = bare("incoming");
    p.labels.insert("tier".to_string(), "front".to_string());

    let (_, state, status) = prefiltered(&p, &snap);
    assert!(status.is_success(), "{status:?}");
    let s = state.read::<SpreadState>(NAME).unwrap();
    let sel = s.constraints[0].selector.as_ref().expect("derived selector");
    let labels = sel.match_labels.as_ref().unwrap();
    assert_eq!(labels.get("app").map(String::as_str), Some("web"));
    assert_eq!(labels.get("tier").map(String::as_str), Some("front"));
}

#[test]
fn a_workload_in_another_namespace_does_not_select_the_pod() {
    let snap = cluster_with_workloads(&[], &[("rs/other/web", workload("other", "web"))]);
    let (_, _, status) = prefiltered(&bare("incoming"), &snap);
    assert!(status.is_skip(), "cross-namespace workload must not apply: {status:?}");
}

#[test]
fn defaulting_none_switches_the_whole_thing_off() {
    let snap = cluster_with_workloads(&[], &[("rs/default/web", workload("default", "web"))]);
    let plugin = PodTopologySpread { defaulting: crate::config::TopologyDefaulting::None };
    let mut state = CycleState::default();
    let (status, _) = plugin.pre_filter(&mut state, &bare("incoming"), &snap);
    assert!(status.is_skip(), "{status:?}");
}
