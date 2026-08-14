use super::*;
use crate::cache::{Cache, PvInfo, PvcInfo, StorageClassInfo};
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
    let (status, _) = pre_filter_impl(&mut state, &pod("p"), &Snapshot::default());
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
    let (status, _) = pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot);
    assert!(!status.is_success());
    assert!(!status.code.is_resolvable_by_preemption());
    assert!(status.reasons[0].contains("unbound immediate"));
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
    let (status, _) = pre_filter_impl(&mut state, &pod_with_pvc("ns", "claim"), &snapshot);
    assert!(status.reasons[0].contains("unbound immediate"));
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
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot);
    assert!(status.is_success());

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success());
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
    pre_filter_impl(&mut state, &p, &snapshot);

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    let status = filter_impl(&state, &p, &n);
    assert!(!status.is_success());
    assert!(status.reasons[0].contains("topology"));
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
    pre_filter_impl(&mut state, &p, &snapshot);

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
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot);
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
    pre_filter_impl(&mut state, &p, &snapshot);
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success());
}

#[test]
fn it_wakes_on_the_events_that_can_actually_progress_binding() {
    let events = events_impl();
    let pvc_updated = ClusterEvent::new(EventResource::PersistentVolumeClaim, ActionType::UPDATE);
    let sc_added = ClusterEvent::new(EventResource::StorageClass, ActionType::ADD);

    assert!(events.iter().any(|e| e.event.matches(&pvc_updated)));
    assert!(events.iter().any(|e| e.event.matches(&sc_added)));
}
