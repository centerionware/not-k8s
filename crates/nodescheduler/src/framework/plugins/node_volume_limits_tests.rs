use super::*;
use crate::cache::{Cache, CsiNodeInfo, PvInfo, PvcInfo, VolumeAttachmentInfo};
use crate::framework::plugins::testutil::pod;
use k8s_openapi::api::core::v1::Node as ApiNode;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::sync::Arc;

fn api_node(name: &str) -> ApiNode {
    ApiNode { metadata: ObjectMeta { name: Some(name.to_string()), ..Default::default() }, ..Default::default() }
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

fn csi_pv(name: &str, driver: &str) -> PvInfo {
    PvInfo {
        name: name.to_string(),
        csi_driver: Some(driver.to_string()),
        csi_volume_handle: Some(format!("handle-{name}")),
        ..Default::default()
    }
}

fn pod_with_pvc(name: &str, uid: &str, pvc: &str) -> Arc<PodInfo> {
    let mut p = pod(name);
    p.uid = uid.to_string();
    p.namespace = "ns".to_string();
    p.pvc_names = vec![pvc.to_string()];
    // Both call sites place this on "n1" — without a node_name, Cache::add_pod
    // silently drops the pod rather than committing it anywhere (see its own
    // doc comment), which would make the node's attached-volume count 0 no
    // matter what the test set up.
    p.node_name = Some("n1".to_string());
    Arc::new(p)
}

#[test]
fn a_pod_with_no_csi_volumes_skips_the_filter() {
    let mut state = CycleState::default();
    let (status, _) = NodeVolumeLimits.pre_filter(&mut state, &pod("p"), &Snapshot::default());
    assert!(status.is_skip());
    assert!(state.filter_skipped(NAME));
}

#[test]
fn a_node_under_its_reported_ceiling_admits_the_pod() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    let mut drivers = std::collections::BTreeMap::new();
    drivers.insert("disk.example.com".to_string(), Some(2));
    cache.upsert_csi_node("n1".to_string(), CsiNodeInfo { drivers });
    cache.upsert_pv("pv-1".to_string(), csi_pv("pv-1", "disk.example.com"));
    cache.upsert_pvc("ns/existing".to_string(), bound_pvc("ns", "existing", "pv-1"));
    cache.add_pod(pod_with_pvc("holder", "uid-holder", "existing"));
    cache.upsert_pv("pv-2".to_string(), csi_pv("pv-2", "disk.example.com"));
    cache.upsert_pvc("ns/newclaim".to_string(), bound_pvc("ns", "newclaim", "pv-2"));

    let incoming = {
        let mut p = pod("incoming");
        p.uid = "uid-incoming".to_string();
        p.namespace = "ns".to_string();
        p.pvc_names = vec!["newclaim".to_string()];
        p
    };

    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = NodeVolumeLimits.pre_filter(&mut state, &incoming, &snapshot);
    assert!(status.is_success());
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(NodeVolumeLimits.filter(&state, &incoming, &n).is_success());
}

#[test]
fn a_node_at_its_reported_ceiling_rejects_one_more() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    let mut drivers = std::collections::BTreeMap::new();
    drivers.insert("disk.example.com".to_string(), Some(1));
    cache.upsert_csi_node("n1".to_string(), CsiNodeInfo { drivers });
    cache.upsert_pv("pv-1".to_string(), csi_pv("pv-1", "disk.example.com"));
    cache.upsert_pvc("ns/existing".to_string(), bound_pvc("ns", "existing", "pv-1"));
    cache.add_pod(pod_with_pvc("holder", "uid-holder", "existing"));
    cache.upsert_pv("pv-2".to_string(), csi_pv("pv-2", "disk.example.com"));
    cache.upsert_pvc("ns/newclaim".to_string(), bound_pvc("ns", "newclaim", "pv-2"));

    let incoming = {
        let mut p = pod("incoming");
        p.uid = "uid-incoming".to_string();
        p.namespace = "ns".to_string();
        p.pvc_names = vec!["newclaim".to_string()];
        p
    };

    let snapshot = cache.snapshot();
    let mut state = CycleState::default();
    let (status, _) = NodeVolumeLimits.pre_filter(&mut state, &incoming, &snapshot);
    assert!(status.is_success(), "PreFilter itself never rejects — only Filter, per node");

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    let result = NodeVolumeLimits.filter(&state, &incoming, &n);
    assert!(!result.is_success());
    assert!(result.code.is_resolvable_by_preemption());
}

#[test]
fn a_driver_with_no_reported_ceiling_is_unbounded() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    let mut drivers = std::collections::BTreeMap::new();
    drivers.insert("disk.example.com".to_string(), None);
    cache.upsert_csi_node("n1".to_string(), CsiNodeInfo { drivers });
    cache.upsert_pv("pv".to_string(), csi_pv("pv", "disk.example.com"));
    cache.upsert_pvc("ns/claim".to_string(), bound_pvc("ns", "claim", "pv"));
    let snapshot = cache.snapshot();
    let mut p = pod("incoming");
    p.namespace = "ns".to_string();
    p.uid = "uid".to_string();
    p.pvc_names = vec!["claim".to_string()];

    let mut state = CycleState::default();
    NodeVolumeLimits.pre_filter(&mut state, &p, &snapshot);
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(NodeVolumeLimits.filter(&state, &p, &n).is_success());
}

#[test]
fn a_node_with_no_csi_node_at_all_enforces_nothing() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("bare"));
    cache.upsert_pv("pv".to_string(), csi_pv("pv", "disk.example.com"));
    cache.upsert_pvc("ns/claim".to_string(), bound_pvc("ns", "claim", "pv"));
    let snapshot = cache.snapshot();

    let mut p = pod("incoming");
    p.namespace = "ns".to_string();
    p.uid = "uid".to_string();
    p.pvc_names = vec!["claim".to_string()];

    let mut state = CycleState::default();
    NodeVolumeLimits.pre_filter(&mut state, &p, &snapshot);
    let n = snapshot.node("bare").unwrap().as_ref().clone();
    assert!(NodeVolumeLimits.filter(&state, &p, &n).is_success());
}

#[test]
fn it_wakes_on_the_events_that_can_actually_change_the_answer() {
    let events = NodeVolumeLimits.events_to_register();
    let deleted = ClusterEvent::new(EventResource::AssignedPod, ActionType::DELETE);
    let pvc_updated = ClusterEvent::new(EventResource::PersistentVolumeClaim, ActionType::UPDATE);
    let csi_node_added = ClusterEvent::new(EventResource::CsiNode, ActionType::ADD);
    let attachment_deleted = ClusterEvent::new(EventResource::VolumeAttachment, ActionType::DELETE);

    assert!(events.iter().any(|e| e.event.matches(&deleted)));
    assert!(events.iter().any(|e| e.event.matches(&pvc_updated)));
    assert!(events.iter().any(|e| e.event.matches(&csi_node_added)));
    assert!(events.iter().any(|e| e.event.matches(&attachment_deleted)));
}

#[test]
fn one_csi_handle_mounted_by_two_pods_consumes_one_slot() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    let mut drivers = std::collections::BTreeMap::new();
    drivers.insert("disk.example.com".to_string(), Some(1));
    cache.upsert_csi_node("n1".to_string(), CsiNodeInfo { drivers });
    cache.upsert_pv("shared".to_string(), csi_pv("shared", "disk.example.com"));
    cache.upsert_pvc("ns/existing".to_string(), bound_pvc("ns", "existing", "shared"));
    cache.upsert_pvc("ns/incoming".to_string(), bound_pvc("ns", "incoming", "shared"));
    cache.add_pod(pod_with_pvc("holder", "holder-uid", "existing"));
    let snapshot = cache.snapshot();
    let mut incoming = pod("incoming");
    incoming.namespace = "ns".to_string();
    incoming.uid = "incoming-uid".to_string();
    incoming.pvc_names = vec!["incoming".to_string()];
    let mut state = CycleState::default();
    NodeVolumeLimits.pre_filter(&mut state, &incoming, &snapshot);
    let node = snapshot.node("n1").unwrap();
    assert!(NodeVolumeLimits.filter(&state, &incoming, node).is_success());
}

#[test]
fn a_lingering_volume_attachment_consumes_a_slot() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    let mut drivers = std::collections::BTreeMap::new();
    drivers.insert("disk.example.com".to_string(), Some(1));
    cache.upsert_csi_node("n1".to_string(), CsiNodeInfo { drivers });
    cache.upsert_pv("attached".to_string(), csi_pv("attached", "disk.example.com"));
    cache.upsert_volume_attachment(
        "va".to_string(),
        VolumeAttachmentInfo {
            node_name: "n1".to_string(),
            attacher: "disk.example.com".to_string(),
            pv_name: Some("attached".to_string()),
        },
    );
    cache.upsert_pv("new".to_string(), csi_pv("new", "disk.example.com"));
    cache.upsert_pvc("ns/new".to_string(), bound_pvc("ns", "new", "new"));
    let snapshot = cache.snapshot();
    let mut incoming = pod("incoming");
    incoming.namespace = "ns".to_string();
    incoming.pvc_names = vec!["new".to_string()];
    let mut state = CycleState::default();
    NodeVolumeLimits.pre_filter(&mut state, &incoming, &snapshot);
    let node = snapshot.node("n1").unwrap();
    assert!(!NodeVolumeLimits.filter(&state, &incoming, node).is_success());
}
