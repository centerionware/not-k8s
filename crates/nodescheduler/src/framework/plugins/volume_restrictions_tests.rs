use super::*;
use crate::cache::{Cache, PvcInfo};
use crate::framework::plugins::testutil::{node, pod};
use k8s_openapi::api::core::v1::Node;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::sync::Arc;

fn api_node(name: &str) -> Node {
    Node { metadata: ObjectMeta { name: Some(name.to_string()), ..Default::default() }, ..Default::default() }
}

fn cluster_with_pvc(pvc: PvcInfo, placed: &[(&str, Arc<PodInfo>)]) -> Snapshot {
    let mut cache = Cache::new();
    for (node_name, p) in placed {
        cache.upsert_node(&api_node(node_name));
        cache.add_pod(p.clone());
    }
    cache.upsert_pvc(pvc.key(), pvc);
    cache.snapshot()
}

fn placed_pod(name: &str, namespace: &str, node_name: &str, pvc: &str) -> Arc<PodInfo> {
    let mut p = pod(name);
    p.namespace = namespace.to_string();
    p.uid = format!("uid-{name}");
    p.node_name = Some(node_name.to_string());
    p.pvc_names = vec![pvc.to_string()];
    Arc::new(p)
}

fn rwop_pvc(namespace: &str, name: &str) -> PvcInfo {
    PvcInfo {
        namespace: namespace.to_string(),
        name: name.to_string(),
        requested_access_modes: vec!["ReadWriteOncePod".to_string()],
        ..Default::default()
    }
}

#[test]
fn a_pod_with_no_volumes_at_all_skips_the_filter() {
    let mut state = CycleState::default();
    let (status, _) = VolumeRestrictions.pre_filter(&mut state, &pod("p"), &Snapshot::default());
    assert!(status.is_skip());
    assert!(state.filter_skipped(NAME));
}

#[test]
fn a_read_write_once_pod_claim_already_in_use_is_rejected_before_any_node_is_asked() {
    let victim = placed_pod("holder", "ns", "n1", "data");
    let snapshot = cluster_with_pvc(rwop_pvc("ns", "data"), &[("n1", victim)]);

    let mut newcomer = pod("newcomer");
    newcomer.namespace = "ns".to_string();
    newcomer.uid = "uid-newcomer".to_string();
    newcomer.pvc_names = vec!["data".to_string()];

    let mut state = CycleState::default();
    let (status, _) = VolumeRestrictions.pre_filter(&mut state, &newcomer, &snapshot);
    assert!(!status.is_success());
    assert!(!status.code.is_resolvable_by_preemption(), "no node fixes this, only freeing the PVC does");
    assert!(status.reasons[0].contains("ReadWriteOncePod"));
}

#[test]
fn an_unclaimed_read_write_once_pod_pvc_is_fine() {
    let snapshot = cluster_with_pvc(rwop_pvc("ns", "data"), &[]);

    let mut p = pod("first");
    p.namespace = "ns".to_string();
    p.uid = "uid-first".to_string();
    p.pvc_names = vec!["data".to_string()];

    let mut state = CycleState::default();
    let (status, _) = VolumeRestrictions.pre_filter(&mut state, &p, &snapshot);
    assert!(status.is_success());
}

#[test]
fn a_pvc_without_read_write_once_pod_never_conflicts() {
    // Ordinary ReadWriteOnce PVCs are VolumeBinding/attach-detach's business,
    // not this plugin's.
    let mut ordinary = rwop_pvc("ns", "data");
    ordinary.requested_access_modes = vec!["ReadWriteOnce".to_string()];
    let holder = placed_pod("holder", "ns", "n1", "data");
    let snapshot = cluster_with_pvc(ordinary, &[("n1", holder)]);

    let mut newcomer = pod("newcomer");
    newcomer.namespace = "ns".to_string();
    newcomer.uid = "uid-newcomer".to_string();
    newcomer.pvc_names = vec!["data".to_string()];

    let mut state = CycleState::default();
    let (status, _) = VolumeRestrictions.pre_filter(&mut state, &newcomer, &snapshot);
    assert!(status.is_success());
}

#[test]
fn two_pods_wanting_the_same_gce_disk_conflict_on_the_same_node() {
    let mut existing = pod("existing");
    existing.legacy_volumes = vec![LegacyVolumeId::GcePersistentDisk {
        pd_name: "disk-1".to_string(),
        read_only: false,
    }];
    let mut n = node("n1");
    n.add_pod(Arc::new(existing), 1);

    let mut incoming = pod("incoming");
    incoming.legacy_volumes = vec![LegacyVolumeId::GcePersistentDisk {
        pd_name: "disk-1".to_string(),
        read_only: false,
    }];
    let mut state = CycleState::default();
    VolumeRestrictions.pre_filter(&mut state, &incoming, &Snapshot::default());

    let status = VolumeRestrictions.filter(&state, &incoming, &n);
    assert!(!status.is_success());
    assert!(status.code.is_resolvable_by_preemption());
}

#[test]
fn two_read_only_claims_on_the_same_gce_disk_do_not_conflict() {
    let mut existing = pod("existing");
    existing.legacy_volumes = vec![LegacyVolumeId::GcePersistentDisk {
        pd_name: "disk-1".to_string(),
        read_only: true,
    }];
    let mut n = node("n1");
    n.add_pod(Arc::new(existing), 1);

    let mut incoming = pod("incoming");
    incoming.legacy_volumes = vec![LegacyVolumeId::GcePersistentDisk {
        pd_name: "disk-1".to_string(),
        read_only: true,
    }];
    let mut state = CycleState::default();
    VolumeRestrictions.pre_filter(&mut state, &incoming, &Snapshot::default());

    assert!(VolumeRestrictions.filter(&state, &incoming, &n).is_success());
}

#[test]
fn different_disks_never_conflict() {
    let mut existing = pod("existing");
    existing.legacy_volumes =
        vec![LegacyVolumeId::GcePersistentDisk { pd_name: "disk-1".to_string(), read_only: false }];
    let mut n = node("n1");
    n.add_pod(Arc::new(existing), 1);

    let mut incoming = pod("incoming");
    incoming.legacy_volumes =
        vec![LegacyVolumeId::GcePersistentDisk { pd_name: "disk-2".to_string(), read_only: false }];
    let mut state = CycleState::default();
    VolumeRestrictions.pre_filter(&mut state, &incoming, &Snapshot::default());

    assert!(VolumeRestrictions.filter(&state, &incoming, &n).is_success());
}

#[test]
fn it_wakes_only_on_an_assigned_pod_delete() {
    let events = VolumeRestrictions.events_to_register();
    let deleted = ClusterEvent::new(EventResource::AssignedPod, ActionType::DELETE);
    let added = ClusterEvent::new(EventResource::AssignedPod, ActionType::ADD);

    assert!(events.iter().any(|e| e.event.matches(&deleted)));
    assert!(!events.iter().any(|e| e.event.matches(&added)));
}
