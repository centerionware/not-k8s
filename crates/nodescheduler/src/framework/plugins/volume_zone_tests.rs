use super::*;
use crate::cache::{Cache, PvInfo, PvcInfo};
use crate::framework::plugins::testutil::pod;
use k8s_openapi::api::core::v1::Node as ApiNode;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

fn api_node(name: &str, zone: Option<&str>) -> ApiNode {
    let labels = zone.map(|z| {
        let mut m = std::collections::BTreeMap::new();
        m.insert("topology.kubernetes.io/zone".to_string(), z.to_string());
        m
    });
    ApiNode {
        metadata: ObjectMeta { name: Some(name.to_string()), labels, ..Default::default() },
        ..Default::default()
    }
}

fn zoned_pv(name: &str, zone: &str) -> PvInfo {
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("topology.kubernetes.io/zone".to_string(), zone.to_string());
    PvInfo { name: name.to_string(), labels, ..Default::default() }
}

fn bound_pvc(namespace: &str, name: &str, pv: &str) -> PvcInfo {
    PvcInfo {
        namespace: namespace.to_string(),
        name: name.to_string(),
        volume_name: Some(pv.to_string()),
        bound: true,
        ..Default::default()
    }
}

#[test]
fn a_pod_with_no_pvcs_skips_the_filter() {
    let mut state = CycleState::default();
    let (status, _) = VolumeZone.pre_filter(&mut state, &pod("p"), &Snapshot::default());
    assert!(status.is_skip());
    assert!(state.filter_skipped(NAME));
}

#[test]
fn a_pv_with_no_zone_label_imposes_no_restriction() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", None));
    cache.upsert_pv("pv-1".to_string(), PvInfo { name: "pv-1".to_string(), ..Default::default() });
    cache.upsert_pvc("ns/claim".to_string(), bound_pvc("ns", "claim", "pv-1"));
    let snapshot = cache.snapshot();

    let mut p = pod("p");
    p.namespace = "ns".to_string();
    p.pvc_names = vec!["claim".to_string()];

    let mut state = CycleState::default();
    VolumeZone.pre_filter(&mut state, &p, &snapshot);
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(VolumeZone.filter(&state, &p, &n).is_success());
}

#[test]
fn a_node_in_the_pvs_zone_is_admitted() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", Some("us-east-1a")));
    cache.upsert_pv("pv-1".to_string(), zoned_pv("pv-1", "us-east-1a"));
    cache.upsert_pvc("ns/claim".to_string(), bound_pvc("ns", "claim", "pv-1"));
    let snapshot = cache.snapshot();

    let mut p = pod("p");
    p.namespace = "ns".to_string();
    p.pvc_names = vec!["claim".to_string()];

    let mut state = CycleState::default();
    VolumeZone.pre_filter(&mut state, &p, &snapshot);
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(VolumeZone.filter(&state, &p, &n).is_success());
}

#[test]
fn a_node_in_the_wrong_zone_is_rejected_unresolvably() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", Some("us-east-1b")));
    cache.upsert_pv("pv-1".to_string(), zoned_pv("pv-1", "us-east-1a"));
    cache.upsert_pvc("ns/claim".to_string(), bound_pvc("ns", "claim", "pv-1"));
    let snapshot = cache.snapshot();

    let mut p = pod("p");
    p.namespace = "ns".to_string();
    p.pvc_names = vec!["claim".to_string()];

    let mut state = CycleState::default();
    VolumeZone.pre_filter(&mut state, &p, &snapshot);
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    let status = VolumeZone.filter(&state, &p, &n);
    assert!(!status.is_success());
    assert!(!status.code.is_resolvable_by_preemption(), "a node's zone never changes");
}

#[test]
fn an_unbound_pvc_has_nothing_to_check_yet() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", Some("us-east-1b")));
    cache.upsert_pvc(
        "ns/claim".to_string(),
        PvcInfo { namespace: "ns".to_string(), name: "claim".to_string(), ..Default::default() },
    );
    let snapshot = cache.snapshot();

    let mut p = pod("p");
    p.namespace = "ns".to_string();
    p.pvc_names = vec!["claim".to_string()];

    let mut state = CycleState::default();
    let (status, _) = VolumeZone.pre_filter(&mut state, &p, &snapshot);
    assert!(status.is_skip());
}

#[test]
fn a_multi_zone_pv_label_is_membership_not_equality() {
    // An in-tree zonal PV can name several zones joined by `__` — exact
    // string equality would reject every node for it.
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1", Some("us-east-1b")));
    cache.upsert_pv("pv-1".to_string(), zoned_pv("pv-1", "us-east-1a__us-east-1b"));
    cache.upsert_pvc("ns/claim".to_string(), bound_pvc("ns", "claim", "pv-1"));
    let snapshot = cache.snapshot();

    let mut p = pod("p");
    p.namespace = "ns".to_string();
    p.pvc_names = vec!["claim".to_string()];

    let mut state = CycleState::default();
    VolumeZone.pre_filter(&mut state, &p, &snapshot);
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(VolumeZone.filter(&state, &p, &n).is_success(), "us-east-1b is one of the PV's member zones");
}

#[test]
fn it_wakes_on_pvc_and_pv_changes() {
    let events = VolumeZone.events_to_register();
    let pvc_updated = ClusterEvent::new(EventResource::PersistentVolumeClaim, ActionType::UPDATE);
    let pv_added = ClusterEvent::new(EventResource::PersistentVolume, ActionType::ADD);

    assert!(events.iter().any(|e| e.event.matches(&pvc_updated)));
    assert!(events.iter().any(|e| e.event.matches(&pv_added)));
}
