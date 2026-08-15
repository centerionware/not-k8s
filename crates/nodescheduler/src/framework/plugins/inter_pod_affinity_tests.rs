//! Tests for InterPodAffinity.
//!
//! Three things here are worth more than the rest, because each is a rule
//! that fails *silently* when it is wrong: the asymmetry between affinity and
//! anti-affinity, the symmetric check against existing pods' own rules, and
//! the fact that a domain spans nodes. A plugin that gets any of them
//! backwards still schedules pods — just onto the wrong ones.

use super::*;
use crate::cache::{Cache, PodInfo};
use crate::framework::plugins::testutil::pod;
use k8s_openapi::api::core::v1::{
    Affinity, Node, NodeStatus, PodAffinity, PodAffinityTerm, PodAntiAffinity,
    WeightedPodAffinityTerm,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use std::collections::BTreeMap;
use std::sync::Arc;

const HOSTNAME: &str = "kubernetes.io/hostname";
const ZONE: &str = "topology.kubernetes.io/zone";

fn selector(pairs: &[(&str, &str)]) -> LabelSelector {
    LabelSelector {
        match_labels: Some(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
        match_expressions: None,
    }
}

fn term(pairs: &[(&str, &str)], topology_key: &str) -> PodAffinityTerm {
    PodAffinityTerm {
        label_selector: Some(selector(pairs)),
        topology_key: topology_key.to_string(),
        ..Default::default()
    }
}

fn api_node(name: &str, labels: &[(&str, &str)]) -> Node {
    Node {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
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

/// An already-running pod with labels, on a node.
fn placed(uid: &str, node: &str, labels: &[(&str, &str)]) -> Arc<PodInfo> {
    Arc::new(PodInfo {
        namespace: "default".to_string(),
        name: uid.to_string(),
        uid: uid.to_string(),
        node_name: Some(node.to_string()),
        labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    })
}

/// An already-running pod that itself declares anti-affinity.
fn placed_with_anti(uid: &str, node: &str, avoid: &[(&str, &str)], key: &str) -> Arc<PodInfo> {
    let mut p = (*placed(uid, node, &[])).clone();
    p.affinity = Some(Box::new(Affinity {
        pod_anti_affinity: Some(PodAntiAffinity {
            required_during_scheduling_ignored_during_execution: Some(vec![term(avoid, key)]),
            ..Default::default()
        }),
        ..Default::default()
    }));
    Arc::new(p)
}

fn incoming(labels: &[(&str, &str)], affinity: Option<Affinity>) -> PodInfo {
    let mut p = pod("incoming");
    p.namespace = "default".to_string();
    p.labels = labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    p.affinity = affinity.map(Box::new);
    p
}

fn with_affinity(t: PodAffinityTerm) -> Affinity {
    Affinity {
        pod_affinity: Some(PodAffinity {
            required_during_scheduling_ignored_during_execution: Some(vec![t]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn with_anti_affinity(t: PodAffinityTerm) -> Affinity {
    Affinity {
        pod_anti_affinity: Some(PodAntiAffinity {
            required_during_scheduling_ignored_during_execution: Some(vec![t]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Two nodes in one zone, one in another, with the given pods placed.
fn cluster(pods: &[(Arc<PodInfo>, &str)]) -> Snapshot {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("a1", &[(HOSTNAME, "a1"), (ZONE, "east")]));
    cache.upsert_node(&api_node("a2", &[(HOSTNAME, "a2"), (ZONE, "east")]));
    cache.upsert_node(&api_node("b1", &[(HOSTNAME, "b1"), (ZONE, "west")]));
    for (p, _) in pods {
        cache.add_pod(p.clone());
    }
    cache.snapshot()
}

fn prefiltered(plugin: &InterPodAffinity, pod: &PodInfo, snap: &Snapshot) -> CycleState {
    let mut state = CycleState::default();
    plugin.pre_filter(&mut state, pod, snap);
    state
}

fn node<'a>(snap: &'a Snapshot, name: &str) -> &'a NodeInfo {
    snap.node(name).expect("node in snapshot")
}

// ── Affinity ────────────────────────────────────────────────────────────

#[test]
fn affinity_is_satisfied_by_one_matching_pod_in_the_domain() {
    let snap = cluster(&[(placed("web", "a1", &[("app", "web")]), "a1")]);
    let p = incoming(&[], Some(with_affinity(term(&[("app", "web")], HOSTNAME))));
    let plugin = InterPodAffinity::default();
    let state = prefiltered(&plugin, &p, &snap);

    assert!(plugin.filter(&state, &p, node(&snap, "a1")).is_success());
    assert!(
        !plugin.filter(&state, &p, node(&snap, "a2")).is_success(),
        "a2 has no matching pod on it, and the domain here is the node"
    );
}

#[test]
fn a_domain_spans_nodes() {
    // THE property that makes this plugin different from a per-node filter.
    // The matching pod is on a1; a2 is a different node but the same zone, so
    // a zone-scoped affinity is satisfied there too.
    let snap = cluster(&[(placed("web", "a1", &[("app", "web")]), "a1")]);
    let p = incoming(&[], Some(with_affinity(term(&[("app", "web")], ZONE))));
    let plugin = InterPodAffinity::default();
    let state = prefiltered(&plugin, &p, &snap);

    assert!(plugin.filter(&state, &p, node(&snap, "a2")).is_success());
    assert!(
        !plugin.filter(&state, &p, node(&snap, "b1")).is_success(),
        "b1 is in the other zone, so its domain has no matching pod"
    );
}

#[test]
fn affinity_with_no_matching_pod_anywhere_rejects_every_node() {
    let snap = cluster(&[(placed("other", "a1", &[("app", "api")]), "a1")]);
    let p = incoming(&[], Some(with_affinity(term(&[("app", "web")], HOSTNAME))));
    let plugin = InterPodAffinity::default();
    let state = prefiltered(&plugin, &p, &snap);

    for n in ["a1", "a2", "b1"] {
        assert!(!plugin.filter(&state, &p, node(&snap, n)).is_success(), "{n}");
    }
}

// ── Anti-affinity ───────────────────────────────────────────────────────

#[test]
fn anti_affinity_is_satisfied_only_where_nothing_matches() {
    // The exact inverse of the affinity case, which is why inverting the
    // comparison is such an easy and invisible mistake.
    let snap = cluster(&[(placed("web", "a1", &[("app", "web")]), "a1")]);
    let p = incoming(&[], Some(with_anti_affinity(term(&[("app", "web")], HOSTNAME))));
    let plugin = InterPodAffinity::default();
    let state = prefiltered(&plugin, &p, &snap);

    assert!(!plugin.filter(&state, &p, node(&snap, "a1")).is_success());
    assert!(plugin.filter(&state, &p, node(&snap, "a2")).is_success());
    assert!(plugin.filter(&state, &p, node(&snap, "b1")).is_success());
}

#[test]
fn zone_scoped_anti_affinity_excludes_the_whole_zone() {
    let snap = cluster(&[(placed("web", "a1", &[("app", "web")]), "a1")]);
    let p = incoming(&[], Some(with_anti_affinity(term(&[("app", "web")], ZONE))));
    let plugin = InterPodAffinity::default();
    let state = prefiltered(&plugin, &p, &snap);

    assert!(!plugin.filter(&state, &p, node(&snap, "a1")).is_success());
    assert!(
        !plugin.filter(&state, &p, node(&snap, "a2")).is_success(),
        "a2 shares the zone with the matching pod"
    );
    assert!(plugin.filter(&state, &p, node(&snap, "b1")).is_success());
}

#[test]
fn a_node_without_the_topology_label_satisfies_neither_rule() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("bare", &[(HOSTNAME, "bare")]));
    cache.add_pod(placed("web", "bare", &[("app", "web")]));
    let snap = cache.snapshot();
    let plugin = InterPodAffinity::default();

    // Zone-scoped rules against a node with no zone label.
    let affin = incoming(&[], Some(with_affinity(term(&[("app", "web")], ZONE))));
    let s1 = prefiltered(&plugin, &affin, &snap);
    assert!(!plugin.filter(&s1, &affin, node(&snap, "bare")).is_success());

    let anti = incoming(&[], Some(with_anti_affinity(term(&[("app", "web")], ZONE))));
    let s2 = prefiltered(&plugin, &anti, &snap);
    assert!(
        !plugin.filter(&s2, &anti, node(&snap, "bare")).is_success(),
        "an unlabelled node is in no domain, so anti-affinity cannot be shown to hold either"
    );
}

// ── The symmetric half ──────────────────────────────────────────────────

#[test]
fn an_existing_pods_own_anti_affinity_blocks_the_incoming_pod() {
    // The check people forget. Our pod declares nothing at all; the running
    // pod's rule is what forbids the placement. Omitting this lets a new pod
    // violate a constraint a running pod declared, which surfaces much later
    // as a rollout that will not converge.
    let snap = cluster(&[(placed_with_anti("guard", "a1", &[("app", "web")], HOSTNAME), "a1")]);
    let p = incoming(&[("app", "web")], None);
    let plugin = InterPodAffinity::default();
    let state = prefiltered(&plugin, &p, &snap);

    assert!(
        !plugin.filter(&state, &p, node(&snap, "a1")).is_success(),
        "a running pod's anti-affinity must forbid placing a matching pod beside it"
    );
    assert!(plugin.filter(&state, &p, node(&snap, "a2")).is_success());
}

#[test]
fn an_existing_pods_anti_affinity_does_not_block_a_pod_it_does_not_match() {
    let snap = cluster(&[(placed_with_anti("guard", "a1", &[("app", "web")], HOSTNAME), "a1")]);
    let p = incoming(&[("app", "batch")], None);
    let plugin = InterPodAffinity::default();
    let state = prefiltered(&plugin, &p, &snap);

    assert!(plugin.filter(&state, &p, node(&snap, "a1")).is_success());
}

// ── Preemption's dry runs ───────────────────────────────────────────────

#[test]
fn hypothetically_removing_the_blocking_pod_admits_the_node() {
    // Preemption's core question, for this plugin. Without the AddPod/RemovePod
    // extensions it evicts a victim and then still refuses to place the pod.
    let blocker = placed("web", "a1", &[("app", "web")]);
    let snap = cluster(&[(blocker.clone(), "a1")]);
    let p = incoming(&[], Some(with_anti_affinity(term(&[("app", "web")], HOSTNAME))));
    let plugin = InterPodAffinity::default();
    let mut state = prefiltered(&plugin, &p, &snap);

    assert!(!plugin.filter(&state, &p, node(&snap, "a1")).is_success());

    plugin.remove_pod(&mut state, &p, &blocker, node(&snap, "a1"));

    assert!(
        plugin.filter(&state, &p, node(&snap, "a1")).is_success(),
        "with the matching pod hypothetically gone, the anti-affinity is satisfied"
    );
}

#[test]
fn hypothetically_removing_a_pod_whose_rule_blocked_us_admits_the_node() {
    // The symmetric half of preemption, and the one most likely to be missed:
    // the victim is not a pod we are avoiding, it is the pod that was
    // avoiding us.
    let guard = placed_with_anti("guard", "a1", &[("app", "web")], HOSTNAME);
    let snap = cluster(&[(guard.clone(), "a1")]);
    let p = incoming(&[("app", "web")], None);
    let plugin = InterPodAffinity::default();
    let mut state = prefiltered(&plugin, &p, &snap);

    assert!(!plugin.filter(&state, &p, node(&snap, "a1")).is_success());

    plugin.remove_pod(&mut state, &p, &guard, node(&snap, "a1"));

    assert!(plugin.filter(&state, &p, node(&snap, "a1")).is_success());
}

#[test]
fn hypothetically_adding_a_pod_can_block_a_node() {
    let snap = cluster(&[]);
    let p = incoming(&[], Some(with_anti_affinity(term(&[("app", "web")], HOSTNAME))));
    let plugin = InterPodAffinity::default();
    let mut state = prefiltered(&plugin, &p, &snap);

    assert!(plugin.filter(&state, &p, node(&snap, "a1")).is_success());

    plugin.add_pod(&mut state, &p, &placed("web", "a1", &[("app", "web")]), node(&snap, "a1"));

    assert!(!plugin.filter(&state, &p, node(&snap, "a1")).is_success());
}

// ── Scoring ─────────────────────────────────────────────────────────────

fn weighted(t: PodAffinityTerm, weight: i32) -> WeightedPodAffinityTerm {
    WeightedPodAffinityTerm { pod_affinity_term: t, weight }
}

#[test]
fn preferred_affinity_raises_a_domains_score() {
    let snap = cluster(&[(placed("web", "a1", &[("app", "web")]), "a1")]);
    let mut p = incoming(&[], None);
    p.affinity = Some(Box::new(Affinity {
        pod_affinity: Some(PodAffinity {
            preferred_during_scheduling_ignored_during_execution: Some(vec![weighted(
                term(&[("app", "web")], HOSTNAME),
                10,
            )]),
            ..Default::default()
        }),
        ..Default::default()
    }));
    let plugin = InterPodAffinity::default();
    let state = prefiltered(&plugin, &p, &snap);

    assert_eq!(plugin.score(&state, &p, node(&snap, "a1")).unwrap(), 10);
    assert_eq!(plugin.score(&state, &p, node(&snap, "a2")).unwrap(), 0);
}

#[test]
fn preferred_anti_affinity_lowers_a_domains_score_below_zero() {
    let snap = cluster(&[(placed("web", "a1", &[("app", "web")]), "a1")]);
    let mut p = incoming(&[], None);
    p.affinity = Some(Box::new(Affinity {
        pod_anti_affinity: Some(PodAntiAffinity {
            preferred_during_scheduling_ignored_during_execution: Some(vec![weighted(
                term(&[("app", "web")], HOSTNAME),
                10,
            )]),
            ..Default::default()
        }),
        ..Default::default()
    }));
    let plugin = InterPodAffinity::default();
    let state = prefiltered(&plugin, &p, &snap);

    assert_eq!(plugin.score(&state, &p, node(&snap, "a1")).unwrap(), -10);
}

#[test]
fn signed_scores_normalize_across_the_whole_range() {
    // The shared divide-by-max helper would clamp both negatives to zero and
    // lose the distinction between them.
    let mut scores = [-10, 0, 10];
    normalize_signed(&mut scores);
    assert_eq!(scores, [0, 50, 100]);
}

#[test]
fn uniform_scores_all_normalize_to_the_maximum() {
    // If no node is preferred over any other, none should be penalised.
    let mut scores = [-5, -5, -5];
    normalize_signed(&mut scores);
    assert_eq!(scores, [100, 100, 100]);
}

// ── Cost ────────────────────────────────────────────────────────────────

#[test]
fn a_pod_with_no_affinity_rules_skips_the_plugin_entirely() {
    // The common case on any cluster. If this ever stops skipping, every pod
    // pays for cross-domain counting it does not use.
    let snap = cluster(&[(placed("web", "a1", &[("app", "web")]), "a1")]);
    let p = incoming(&[], None);
    let plugin = InterPodAffinity::default();
    let mut state = CycleState::default();

    let (status, _) = plugin.pre_filter(&mut state, &p, &snap);

    assert!(status.is_skip());
    assert!(state.filter_skipped(NAME));
    assert!(state.score_skipped(NAME));
}

#[test]
fn a_pod_with_no_rules_is_still_checked_against_existing_pods_rules() {
    // The skip above must not skip the symmetric half — that check applies to
    // every pod, including one that declares nothing.
    let snap = cluster(&[(placed_with_anti("guard", "a1", &[("app", "web")], HOSTNAME), "a1")]);
    let p = incoming(&[("app", "web")], None);
    let plugin = InterPodAffinity::default();
    let mut state = CycleState::default();

    let (status, _) = plugin.pre_filter(&mut state, &p, &snap);

    assert!(!status.is_skip(), "a pod an existing rule forbids cannot skip filtering");
    assert!(!state.filter_skipped(NAME));
    assert!(!plugin.filter(&state, &p, node(&snap, "a1")).is_success());
}

#[test]
fn it_registers_exactly_the_events_that_can_unstick_a_rejected_pod() {
    let events = InterPodAffinity::default().events_to_register();
    let pairs: Vec<(EventResource, ActionType)> =
        events.iter().map(|e| (e.event.resource, e.event.action)).collect();

    assert_eq!(
        pairs,
        vec![
            (
                EventResource::AssignedPod,
                ActionType::ADD | ActionType::DELETE | ActionType::UPDATE_POD_LABEL
            ),
            (EventResource::Node, ActionType::ADD | ActionType::UPDATE_NODE_LABEL),
        ]
    );
}

#[test]
fn a_node_heartbeat_does_not_wake_a_pod_this_plugin_rejected() {
    let heartbeat = ClusterEvent::new(EventResource::Node, ActionType::UPDATE_NODE_CONDITION);
    for reg in InterPodAffinity::default().events_to_register() {
        assert!(!reg.event.matches(&heartbeat));
    }
}

// ── namespaceSelector ───────────────────────────────────────────────────
//
// The rule is resolved against real Namespace labels now. An earlier version
// had no Namespace watch and made every such term match everything, on the
// argument that over-matching only refuses a placement while under-matching
// silently disables a rule. That is the right ranking of two wrong answers,
// and neither is parity — these tests exist so it cannot quietly return.

fn cluster_with_namespaces(
    pods: &[(Arc<PodInfo>, &str)],
    namespaces: &[(&str, &[(&str, &str)])],
) -> Snapshot {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("a1", &[(HOSTNAME, "a1"), (ZONE, "east")]));
    cache.upsert_node(&api_node("a2", &[(HOSTNAME, "a2"), (ZONE, "east")]));
    cache.upsert_node(&api_node("b1", &[(HOSTNAME, "b1"), (ZONE, "west")]));
    for (name, labels) in namespaces {
        cache.upsert_namespace(
            name,
            labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        );
    }
    for (p, _) in pods {
        cache.add_pod(p.clone());
    }
    cache.snapshot()
}

fn placed_in(ns: &str, uid: &str, node: &str, labels: &[(&str, &str)]) -> Arc<PodInfo> {
    let mut p = (*placed(uid, node, labels)).clone();
    p.namespace = ns.to_string();
    Arc::new(p)
}

fn term_with_ns_selector(
    pairs: &[(&str, &str)],
    key: &str,
    ns_pairs: &[(&str, &str)],
) -> PodAffinityTerm {
    let mut t = term(pairs, key);
    t.namespace_selector = Some(selector(ns_pairs));
    t
}

#[test]
fn a_namespace_selector_matches_only_the_namespaces_whose_labels_satisfy_it() {
    // `db` is in a namespace labelled env=prod, so the affinity is satisfied
    // on a1 and nowhere else.
    let db = placed_in("prod-a", "db", "a1", &[("app", "db")]);
    let snap = cluster_with_namespaces(
        &[(db, "a1")],
        &[("prod-a", &[("env", "prod")]), ("staging", &[("env", "staging")])],
    );
    let plugin = InterPodAffinity::default();
    let p = incoming(
        &[("app", "web")],
        Some(with_affinity(term_with_ns_selector(&[("app", "db")], HOSTNAME, &[("env", "prod")]))),
    );
    let state = prefiltered(&plugin, &p, &snap);

    assert!(plugin.filter(&state, &p, node(&snap, "a1")).is_success());
    assert!(!plugin.filter(&state, &p, node(&snap, "b1")).is_success());
}

#[test]
fn a_pod_in_a_namespace_the_selector_rejects_is_not_counted() {
    // Same shape, but `db` lives in staging. Matching it anyway is the
    // over-matching failure the old fail-open behaviour had: an affinity that
    // is satisfied by a pod the author never meant to name.
    let db = placed_in("staging", "db", "a1", &[("app", "db")]);
    let snap = cluster_with_namespaces(
        &[(db, "a1")],
        &[("prod-a", &[("env", "prod")]), ("staging", &[("env", "staging")])],
    );
    let plugin = InterPodAffinity::default();
    let p = incoming(
        &[("app", "web")],
        Some(with_affinity(term_with_ns_selector(&[("app", "db")], HOSTNAME, &[("env", "prod")]))),
    );
    let state = prefiltered(&plugin, &p, &snap);

    for name in ["a1", "a2", "b1"] {
        assert!(
            !plugin.filter(&state, &p, node(&snap, name)).is_success(),
            "{name} must not satisfy an affinity whose namespaceSelector excludes the only match"
        );
    }
}

#[test]
fn an_existing_pods_anti_affinity_namespace_selector_is_resolved_too() {
    // The symmetric direction — their rule, our namespace. Getting this one
    // wrong lets a new pod land next to a running pod that forbade it.
    let mut noisy = (*placed_in("prod-a", "noisy", "a1", &[])).clone();
    noisy.affinity = Some(Box::new(Affinity {
        pod_anti_affinity: Some(PodAntiAffinity {
            required_during_scheduling_ignored_during_execution: Some(vec![
                term_with_ns_selector(&[("app", "web")], HOSTNAME, &[("env", "prod")]),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }));
    let snap = cluster_with_namespaces(
        &[(Arc::new(noisy), "a1")],
        &[("default", &[("env", "prod")]), ("prod-a", &[("env", "prod")])],
    );
    let plugin = InterPodAffinity::default();
    let p = incoming(&[("app", "web")], None);
    let state = prefiltered(&plugin, &p, &snap);

    assert!(
        !plugin.filter(&state, &p, node(&snap, "a1")).is_success(),
        "the running pod's anti-affinity selects our namespace, so a1 is forbidden"
    );
    assert!(plugin.filter(&state, &p, node(&snap, "a2")).is_success());
}

#[test]
fn a_namespace_with_no_labels_known_satisfies_no_selector() {
    // A namespace the watch has not delivered yet must not match — failing
    // open here is what the old behaviour did, and it is the whole point of
    // the change.
    let db = placed_in("prod-a", "db", "a1", &[("app", "db")]);
    let snap = cluster_with_namespaces(&[(db, "a1")], &[]);
    let plugin = InterPodAffinity::default();
    let p = incoming(
        &[("app", "web")],
        Some(with_affinity(term_with_ns_selector(&[("app", "db")], HOSTNAME, &[("env", "prod")]))),
    );
    let state = prefiltered(&plugin, &p, &snap);
    assert!(!plugin.filter(&state, &p, node(&snap, "a1")).is_success());
}
