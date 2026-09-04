use super::*;
use crate::cache::{Cache, PvInfo, PvcInfo, StorageClassInfo};
use std::collections::HashSet;
use crate::framework::plugins::testutil::pod;
use k8s_openapi::api::core::v1::{
    Node as ApiNode, NodeSelector, NodeSelectorRequirement, NodeSelectorTerm,
    TopologySelectorLabelRequirement,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

fn api_node(name: &str, labels: &[(&str, &str)]) -> ApiNode {
    let mut m = std::collections::BTreeMap::new();
    for (k, v) in labels {
        m.insert(k.to_string(), v.to_string());
    }
    ApiNode {
        metadata: ObjectMeta { name: Some(name.to_string()), labels: Some(m), ..Default::default() },
        ..Default::default()
    }
}

fn pod_with_pvc(namespace: &str, pvc: &str) -> PodInfo {
    let mut p = pod("p");
    p.namespace = namespace.to_string();
    p.pvc_names = vec![pvc.to_string()];
    p
}

fn no_excluded() -> HashSet<String> {
    HashSet::new()
}

fn wfc_class(name: &str, provisioner: &str) -> StorageClassInfo {
    StorageClassInfo {
        name: name.to_string(),
        provisioner: provisioner.to_string(),
        wait_for_first_consumer: true,
        allowed_topologies: Vec::new(),
    }
}

#[test]
fn a_pod_with_no_pvcs_skips_the_plugin() {
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &pod("p"), &Snapshot::default(), &no_excluded());
    assert!(status.is_skip());
    assert!(state.filter_skipped(NAME));
}

#[test]
fn an_unbound_pvc_naming_no_storage_class_blocks_the_pod_outright() {
    let mut cache = Cache::new();
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo { namespace: "ns".to_string(), name: "claim".to_string(), ..Default::default() },
    );
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot, &no_excluded());
    assert!(!status.is_success());
    assert!(!status.code.is_resolvable_by_preemption());
    assert!(status.reasons[0].contains("unbound immediate"));
}

#[test]
fn a_lost_or_terminating_pvc_is_rejected_before_binding() {
    for pvc in [
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            phase: "Lost".to_string(),
            volume_name: Some("gone".to_string()),
            ..Default::default()
        },
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            deleting: true,
            ..Default::default()
        },
    ] {
        let mut cache = Cache::new();
        cache.upsert_pvc("ns/claim".to_string(), pvc);
        let mut state = CycleState::default();
        let (status, _) = pre_filter_impl(
            &mut state,
            &pod_with_pvc("ns", "claim"),
            &cache.snapshot(),
            &no_excluded(),
        );
        assert!(!status.is_success());
        assert!(!status.code.is_resolvable_by_preemption());
    }
}

#[test]
fn a_generic_ephemeral_claim_must_be_owned_by_this_pod_uid() {
    let mut cache = Cache::new();
    cache.upsert_pvc(
        "ns/p-cache".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "p-cache".to_string(),
            controller_owner_uid: Some("someone-else".to_string()),
            ..Default::default()
        },
    );
    let mut p = pod_with_pvc("ns", "p-cache");
    p.uid = "this-pod".to_string();
    p.ephemeral_pvc_names = vec!["p-cache".to_string()];
    let mut state = CycleState::default();
    let (status, _) =
        pre_filter_impl(&mut state, &p, &cache.snapshot(), &no_excluded());
    assert!(!status.is_success());
    assert!(status.reasons[0].contains("pod is not owner"));
}

#[test]
fn an_unbound_pvc_on_an_immediate_storage_class_blocks_the_pod_outright() {
    let mut cache = Cache::new();
    cache.upsert_storage_class(
        "immediate".to_string(),
        StorageClassInfo { name: "immediate".to_string(), provisioner: "disk.example.com".to_string(), ..Default::default() },
    );
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            storage_class_name: Some("immediate".to_string()),
            ..Default::default()
        },
    );
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot, &no_excluded());
    assert!(status.reasons[0].contains("unbound immediate"));
}

#[test]
fn a_pvc_waits_for_a_storage_class_that_has_not_reached_the_cache_yet() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            storage_class_name: Some("late-class".to_string()),
            ..Default::default()
        },
    );

    let mut state = CycleState::default();
    let (status, _) =
        pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &cache.snapshot(), &no_excluded());
    assert_eq!(status.code, crate::framework::status::Code::Pending);
    assert_eq!(status.plugin, NAME);
    assert!(status.reasons[0].contains("late-class"));
}

#[test]
fn a_wait_for_first_consumer_pvc_with_no_topology_restriction_fits_any_node() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_storage_class("wfc".to_string(), wfc_class("wfc", "disk.example.com"));
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            storage_class_name: Some("wfc".to_string()),
            ..Default::default()
        },
    );
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success());

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success());
}

#[test]
fn a_no_provisioner_class_with_no_static_pv_rejects_without_preemption() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_storage_class(
        "local".to_string(),
        wfc_class("local", "kubernetes.io/no-provisioner"),
    );
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            storage_class_name: Some("local".to_string()),
            ..Default::default()
        },
    );
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    assert!(pre_filter_impl(&mut state, &p, &snapshot, &no_excluded()).0.is_success());
    let status = filter_impl(&state, &p, snapshot.node("n1").unwrap());
    assert!(!status.is_success());
    assert!(!status.code.is_resolvable_by_preemption());
}

#[test]
fn a_node_outside_the_storage_classs_allowed_topology_is_rejected() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[("topology.kubernetes.io/zone", "west")]));
    let mut sc = wfc_class("wfc", "disk.example.com");
    sc.allowed_topologies = vec![k8s_openapi::api::core::v1::TopologySelectorTerm {
        match_label_expressions: Some(vec![TopologySelectorLabelRequirement {
            key: "topology.kubernetes.io/zone".to_string(),
            values: vec!["east".to_string()],
        }]),
    }];
    cache.upsert_storage_class("wfc".to_string(), sc);
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            storage_class_name: Some("wfc".to_string()),
            ..Default::default()
        },
    );
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    let status = filter_impl(&state, &p, &n);
    assert!(!status.is_success());
    assert!(!status.code.is_resolvable_by_preemption());
    assert!(status.reasons[0].contains("topology"));
}

#[test]
fn a_retry_honours_the_node_already_selected_for_dynamic_provisioning() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("selected", &[]));
    cache.upsert_node(&api_node("other", &[]));
    cache.upsert_storage_class("wfc".to_string(), wfc_class("wfc", "disk.example.com"));
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            storage_class_name: Some("wfc".to_string()),
            selected_node: Some("selected".to_string()),
            ..Default::default()
        },
    );
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    assert!(pre_filter_impl(&mut state, &p, &snapshot, &no_excluded()).0.is_success());

    assert!(filter_impl(&state, &p, snapshot.node("selected").unwrap()).is_success());
    let rejected = filter_impl(&state, &p, snapshot.node("other").unwrap());
    assert!(!rejected.is_success());
    assert!(!rejected.code.is_resolvable_by_preemption());
}

#[test]
fn a_selected_node_continues_provisioning_instead_of_rematching_a_new_static_pv() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("selected", &[]));
    cache.upsert_storage_class("wfc".to_string(), wfc_class("wfc", "disk.example.com"));
    let mut pv = matching_pv("appeared-later", 10);
    pv.storage_class_name = "wfc".to_string();
    cache.upsert_pv("appeared-later".to_string(), pv);
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            storage_class_name: Some("wfc".to_string()),
            selected_node: Some("selected".to_string()),
            requested_access_modes: vec!["ReadWriteOnce".to_string()],
            requested_bytes: 10,
            ..Default::default()
        },
    );
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    assert!(pre_filter_impl(&mut state, &p, &snapshot, &no_excluded()).0.is_success());
    let wanted = state.read::<WantedPvcs>(NAME).unwrap();
    assert!(matches!(wanted.0[0], PvcConstraint::Delayed { .. }));
}

#[test]
fn a_node_inside_the_allowed_topology_is_admitted() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[("topology.kubernetes.io/zone", "east")]));
    let mut sc = wfc_class("wfc", "disk.example.com");
    sc.allowed_topologies = vec![k8s_openapi::api::core::v1::TopologySelectorTerm {
        match_label_expressions: Some(vec![TopologySelectorLabelRequirement {
            key: "topology.kubernetes.io/zone".to_string(),
            values: vec!["east".to_string()],
        }]),
    }];
    cache.upsert_storage_class("wfc".to_string(), sc);
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            storage_class_name: Some("wfc".to_string()),
            ..Default::default()
        },
    );
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success());
}

#[test]
fn a_bound_pvcs_node_affinity_is_enforced() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("wrong-zone", &[("topology.kubernetes.io/zone", "west")]));
    cache.upsert_node(&api_node("right-zone", &[("topology.kubernetes.io/zone", "east")]));
    cache.upsert_pv(
        "pv-1".to_string(),
        PvInfo {
            name: "pv-1".to_string(),
            node_affinity: Some(Box::new(NodeSelector {
                node_selector_terms: vec![NodeSelectorTerm {
                    match_expressions: Some(vec![NodeSelectorRequirement {
                        key: "topology.kubernetes.io/zone".to_string(),
                        operator: "In".to_string(),
                        values: Some(vec!["east".to_string()]),
                    }]),
                    match_fields: None,
                }],
            })),
            ..Default::default()
        },
    );
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            volume_name: Some("pv-1".to_string()),
            bound: true,
            ..Default::default()
        },
    );
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success(), "a bound PVC never blocks PreFilter itself");

    let wrong = snapshot.node("wrong-zone").unwrap().as_ref().clone();
    let right = snapshot.node("right-zone").unwrap().as_ref().clone();
    assert!(!filter_impl(&state, &p, &wrong).is_success());
    assert!(filter_impl(&state, &p, &right).is_success());
}

#[test]
fn a_bound_pvc_with_no_node_affinity_at_all_fits_any_node() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_pv("pv-1".to_string(), PvInfo { name: "pv-1".to_string(), ..Default::default() });
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo {
            namespace: "ns".to_string(),
            name: "claim".to_string(),
            volume_name: Some("pv-1".to_string()),
            bound: true,
            ..Default::default()
        },
    );
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success());
}

// ── Static PVs ───────────────────────────────────────────────────────────

fn matching_pv(name: &str, requested_bytes: i64) -> PvInfo {
    PvInfo {
        name: name.to_string(),
        access_modes: vec!["ReadWriteOnce".to_string()],
        capacity_bytes: requested_bytes,
        phase: "Available".to_string(),
        ..Default::default()
    }
}

fn pvc_wanting(namespace: &str, name: &str, requested_bytes: i64) -> PvcInfo {
    PvcInfo {
        namespace: namespace.to_string(),
        name: name.to_string(),
        requested_access_modes: vec!["ReadWriteOnce".to_string()],
        requested_bytes,
        ..Default::default()
    }
}

#[test]
fn a_matching_unclaimed_static_pv_is_preferred_over_dynamic_provisioning() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    // A StorageClass is present too — if this test passed anyway, it might
    // only prove dynamic provisioning by accident.
    cache.upsert_storage_class("wfc".to_string(), wfc_class("wfc", "disk.example.com"));
    cache.upsert_pv("pv-1".to_string(), matching_pv("pv-1", 10));
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success());

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success());

    let wanted = state.read::<WantedPvcs>(NAME).unwrap();
    assert!(matches!(wanted.0[0], PvcConstraint::Static { .. }), "a matching static PV must win over WaitForFirstConsumer provisioning");
}

#[test]
fn the_smallest_sufficient_static_pv_is_selected_deterministically() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_pv("large".to_string(), matching_pv("large", 100));
    cache.upsert_pv("small".to_string(), matching_pv("small", 10));
    cache.upsert_pv("medium".to_string(), matching_pv("medium", 50));
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();

    let candidates = find_static_candidates(snapshot.pvc("ns", "claim").unwrap(), &snapshot, &no_excluded());
    let by_node = resolve_by_node(&candidates, &snapshot);
    assert_eq!(by_node.get("n1").map(String::as_str), Some("small"));
}

#[test]
fn two_claims_in_one_pod_cannot_both_reserve_the_same_static_pv() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_pv("only".to_string(), matching_pv("only", 10));
    cache.upsert_pvc("ns/a".to_string(), pvc_wanting("ns", "a", 10));
    cache.upsert_pvc("ns/b".to_string(), pvc_wanting("ns", "b", 10));
    let snapshot = cache.snapshot();
    let mut p = pod_with_pvc("ns", "a");
    p.pvc_names.push("b".to_string());
    let mut state = CycleState::default();

    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success());
    let node = snapshot.node("n1").unwrap();
    let rejected = filter_impl(&state, &p, node);
    assert!(
        !rejected.is_success(),
        "one PV cannot satisfy two distinct PVCs in the same pod"
    );
    assert!(!rejected.code.is_resolvable_by_preemption());
}

#[test]
fn two_claims_receive_two_distinct_static_pvs() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_pv("small".to_string(), matching_pv("small", 10));
    cache.upsert_pv("large".to_string(), matching_pv("large", 20));
    cache.upsert_pvc("ns/a".to_string(), pvc_wanting("ns", "a", 10));
    cache.upsert_pvc("ns/b".to_string(), pvc_wanting("ns", "b", 10));
    let snapshot = cache.snapshot();
    let mut p = pod_with_pvc("ns", "a");
    p.pvc_names.push("b".to_string());
    let mut state = CycleState::default();

    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success());
    let node = snapshot.node("n1").unwrap();
    assert!(filter_impl(&state, &p, node).is_success());
    let picked = reserve_impl(state.read::<WantedPvcs>(NAME).unwrap(), "n1").unwrap();
    let unique: HashSet<_> = picked.values().collect();
    assert_eq!(picked.len(), 2);
    assert_eq!(unique.len(), 2);
}

#[test]
fn a_pv_prebound_to_the_claim_wins_even_when_a_smaller_volume_is_free() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_pv("small".to_string(), matching_pv("small", 10));
    let mut prebound = matching_pv("prebound", 100);
    prebound.claim_ref = Some(("ns".to_string(), "claim".to_string()));
    cache.upsert_pv("prebound".to_string(), prebound);
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();

    let candidates = find_static_candidates(snapshot.pvc("ns", "claim").unwrap(), &snapshot, &no_excluded());
    let by_node = resolve_by_node(&candidates, &snapshot);
    assert_eq!(by_node.get("n1").map(String::as_str), Some("prebound"));
}

#[test]
fn a_user_prebound_pv_bypasses_ordinary_class_access_mode_and_selector_checks() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    let mut prebound = matching_pv("prebound", 100);
    prebound.claim_ref = Some(("ns".to_string(), "claim".to_string()));
    prebound.phase = "Bound".to_string();
    prebound.storage_class_name = "different-class".to_string();
    prebound.access_modes.clear();
    cache.upsert_pv("prebound".to_string(), prebound);
    let mut pvc = pvc_wanting("ns", "claim", 10);
    pvc.storage_class_name = Some("wanted-class".to_string());
    pvc.selector = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
        match_labels: Some(std::collections::BTreeMap::from([(
            "tier".to_string(),
            "gold".to_string(),
        )])),
        match_expressions: None,
    });
    cache.upsert_pvc("ns/claim".to_string(), pvc);
    let snapshot = cache.snapshot();

    let candidates = find_static_candidates(
        snapshot.pvc("ns", "claim").unwrap(),
        &snapshot,
        &no_excluded(),
    );
    let by_node = resolve_by_node(&candidates, &snapshot);
    assert_eq!(by_node.get("n1").map(String::as_str), Some("prebound"));
}

#[test]
fn a_static_pv_too_small_is_not_a_candidate() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_pv("pv-1".to_string(), matching_pv("pv-1", 5));
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot, &no_excluded());
    // No StorageClass either, so falling through lands on the same
    // "unbound immediate" rejection the no-static-match path always does.
    assert!(status.reasons[0].contains("unbound immediate"));
}

#[test]
fn a_released_static_pv_is_not_a_candidate() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    let mut pv = matching_pv("pv-1", 10);
    pv.phase = "Released".to_string();
    cache.upsert_pv("pv-1".to_string(), pv);
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot, &no_excluded());
    assert!(
        status.reasons[0].contains("unbound immediate"),
        "a Released PV never actually completes a bind and must not be offered as a candidate"
    );
}

#[test]
fn a_static_pv_with_a_mismatched_volume_mode_is_not_a_candidate() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    let mut pv = matching_pv("pv-1", 10);
    pv.volume_mode = "Block".to_string();
    cache.upsert_pv("pv-1".to_string(), pv);
    let mut pvc = pvc_wanting("ns", "claim", 10);
    pvc.volume_mode = "Filesystem".to_string();
    cache.upsert_pvc("ns/claim".to_string(), pvc);
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot, &no_excluded());
    assert!(
        status.reasons[0].contains("unbound immediate"),
        "binding a Block PV to a Filesystem claim fails at mount time, not match time — must not be a candidate"
    );
}

#[test]
fn a_static_pv_already_claimed_by_another_pvc_is_not_a_candidate() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    let mut pv = matching_pv("pv-1", 10);
    pv.claim_ref = Some(("ns".to_string(), "someone-else".to_string()));
    cache.upsert_pv("pv-1".to_string(), pv);
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot, &no_excluded());
    assert!(status.reasons[0].contains("unbound immediate"), "a PV claimed by a different PersistentVolumeClaim is not free");
}

#[test]
fn a_static_pv_with_a_malformed_but_present_claim_ref_is_not_treated_as_free() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    let mut pv = matching_pv("pv-1", 10);
    pv.claim_ref_present = true;
    cache.upsert_pv("pv-1".to_string(), pv);
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(
        &mut state,
        &pod_with_pvc("ns", "claim"),
        &snapshot,
        &no_excluded(),
    );
    assert!(status.reasons[0].contains("unbound immediate"));
}

#[test]
fn a_static_pv_claimed_by_an_old_incarnation_of_the_same_pvc_is_not_a_candidate() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    let mut pv = matching_pv("pv-1", 10);
    pv.claim_ref = Some(("ns".to_string(), "claim".to_string()));
    pv.claim_ref_uid = Some("deleted-pvc-uid".to_string());
    cache.upsert_pv("pv-1".to_string(), pv);
    let mut pvc = pvc_wanting("ns", "claim", 10);
    pvc.uid = "new-pvc-uid".to_string();
    cache.upsert_pvc("ns/claim".to_string(), pvc);
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(
        &mut state,
        &pod_with_pvc("ns", "claim"),
        &snapshot,
        &no_excluded(),
    );
    assert!(
        status.reasons[0].contains("unbound immediate"),
        "namespace/name equality cannot override a mismatched claimRef UID"
    );
}

#[test]
fn a_terminating_static_pv_is_not_a_candidate() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    let mut pv = matching_pv("pv-1", 10);
    pv.deleting = true;
    cache.upsert_pv("pv-1".to_string(), pv);
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(
        &mut state,
        &pod_with_pvc("ns", "claim"),
        &snapshot,
        &no_excluded(),
    );
    assert!(status.reasons[0].contains("unbound immediate"));
}

#[test]
fn a_static_pv_using_the_disabled_volume_attributes_class_feature_is_not_a_candidate() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    let mut pv = matching_pv("pv-1", 10);
    pv.volume_attributes_class_name = Some("fast-iops".to_string());
    cache.upsert_pv("pv-1".to_string(), pv);
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(
        &mut state,
        &pod_with_pvc("ns", "claim"),
        &snapshot,
        &no_excluded(),
    );
    assert!(status.reasons[0].contains("unbound immediate"));
}

#[test]
fn a_static_pv_a_pods_own_pvc_selector_does_not_match_is_not_a_candidate() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_pv("pv-1".to_string(), matching_pv("pv-1", 10));
    let mut pvc = pvc_wanting("ns", "claim", 10);
    pvc.selector = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
        match_labels: Some(std::collections::BTreeMap::from([("tier".to_string(), "gold".to_string())])),
        match_expressions: None,
    });
    cache.upsert_pvc("ns/claim".to_string(), pvc);
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot, &no_excluded());
    assert!(status.reasons[0].contains("unbound immediate"), "the PV lacks the label the PVC's selector requires");
}

#[test]
fn a_static_pv_excluded_by_another_pods_in_flight_reservation_is_skipped() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_pv("pv-1".to_string(), matching_pv("pv-1", 10));
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let mut excluded = HashSet::new();
    excluded.insert("pv-1".to_string());
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot, &excluded);
    assert!(status.reasons[0].contains("unbound immediate"), "the only matching PV is already tentatively claimed elsewhere");
}

#[test]
fn a_static_pvs_node_affinity_is_checked_per_node() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("wrong-zone", &[("topology.kubernetes.io/zone", "west")]));
    cache.upsert_node(&api_node("right-zone", &[("topology.kubernetes.io/zone", "east")]));
    let mut pv = matching_pv("pv-1", 10);
    pv.node_affinity = Some(Box::new(NodeSelector {
        node_selector_terms: vec![NodeSelectorTerm {
            match_expressions: Some(vec![NodeSelectorRequirement {
                key: "topology.kubernetes.io/zone".to_string(),
                operator: "In".to_string(),
                values: Some(vec!["east".to_string()]),
            }]),
            match_fields: None,
        }],
    }));
    cache.upsert_pv("pv-1".to_string(), pv);
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());

    let wrong = snapshot.node("wrong-zone").unwrap().as_ref().clone();
    let right = snapshot.node("right-zone").unwrap().as_ref().clone();
    assert!(!filter_impl(&state, &p, &wrong).is_success());
    assert!(filter_impl(&state, &p, &right).is_success());
}

#[test]
fn a_pre_bound_pvc_considers_only_the_pv_it_names() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    // A second, otherwise-perfectly-matching PV exists too — if this test
    // passed anyway, it might only prove the general scan, not that a
    // pre-bound PVC is pinned to its own named PV specifically.
    cache.upsert_pv("pv-decoy".to_string(), matching_pv("pv-decoy", 10));
    cache.upsert_pv("pv-1".to_string(), matching_pv("pv-1", 10));
    let mut pvc = pvc_wanting("ns", "claim", 10);
    pvc.volume_name = Some("pv-1".to_string());
    cache.upsert_pvc("ns/claim".to_string(), pvc);
    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot, &no_excluded());

    let wanted = state.read::<WantedPvcs>(NAME).unwrap();
    let PvcConstraint::Static { by_node, .. } = &wanted.0[0] else { panic!("expected Static") };
    assert_eq!(by_node.get("n1"), Some(&"pv-1".to_string()), "the decoy PV must never be picked for a pre-bound claim");
}

#[test]
fn reserve_picks_the_candidate_usable_on_the_winning_node() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    cache.upsert_pv("pv-1".to_string(), matching_pv("pv-1", 10));
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let wanted = state.read::<WantedPvcs>(NAME).unwrap();

    let picked = reserve_impl(&wanted, "n1").unwrap();
    assert_eq!(picked.get("ns/claim"), Some(&"pv-1".to_string()));
}

#[test]
fn reserve_errors_if_the_winning_node_has_no_usable_candidate() {
    // Shouldn't happen in a real cycle (Filter would already have rejected
    // this node) — this is the "internal: plugin bug" guard, not a
    // scheduling outcome.
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", &[]));
    let mut pv = matching_pv("pv-1", 10);
    pv.node_affinity = Some(Box::new(NodeSelector {
        node_selector_terms: vec![NodeSelectorTerm {
            match_expressions: Some(vec![NodeSelectorRequirement {
                key: "topology.kubernetes.io/zone".to_string(),
                operator: "In".to_string(),
                values: Some(vec!["nowhere".to_string()]),
            }]),
            match_fields: None,
        }],
    }));
    cache.upsert_pv("pv-1".to_string(), pv);
    cache.upsert_pvc("ns/claim".to_string(), pvc_wanting("ns", "claim", 10));
    let snapshot = cache.snapshot();
    let p = pod_with_pvc("ns", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let wanted = state.read::<WantedPvcs>(NAME).unwrap();

    assert!(reserve_impl(&wanted, "n1").is_err());
}

#[test]
fn it_wakes_on_the_events_that_can_actually_progress_binding() {
    let events = events_impl();
    let registered: Vec<_> = events.into_iter().map(|e| e.event).collect();

    assert_eq!(
        registered,
        vec![
            ClusterEvent::new(EventResource::Node, ActionType::ADD | ActionType::UPDATE_NODE_LABEL),
            ClusterEvent::new(
                EventResource::PersistentVolumeClaim,
                ActionType::ADD | ActionType::UPDATE,
            ),
            ClusterEvent::new(
                EventResource::PersistentVolume,
                ActionType::ADD | ActionType::UPDATE,
            ),
            ClusterEvent::new(
                EventResource::StorageClass,
                ActionType::ADD | ActionType::UPDATE,
            ),
            ClusterEvent::new(EventResource::CsiDriver, ActionType::UPDATE),
            ClusterEvent::new(
                EventResource::CsiStorageCapacity,
                ActionType::ADD | ActionType::UPDATE,
            ),
        ]
    );
}
