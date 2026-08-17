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
fn a_read_write_once_pod_claim_already_in_use_is_rejected_by_filter_on_every_node() {
    // Not a PreFilter rejection — see the module header for why. PreFilter
    // only records the conflict; Filter is what rejects, identically for
    // every node, and — unlike the old PreFilter-level rejection — as a
    // resolvable status, because it actually is one: evicting `holder`
    // fixes it.
    let holder = placed_pod("holder", "ns", "n1", "data");
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_node(&api_node("n2"));
    cache.add_pod(holder);
    let pvc = rwop_pvc("ns", "data");
    cache.upsert_pvc(pvc.key(), pvc);
    let snapshot = cache.snapshot();

    let mut newcomer = pod("newcomer");
    newcomer.namespace = "ns".to_string();
    newcomer.uid = "uid-newcomer".to_string();
    newcomer.pvc_names = vec!["data".to_string()];

    let mut state = CycleState::default();
    let (status, _) = VolumeRestrictions.pre_filter(&mut state, &newcomer, &snapshot);
    assert!(status.is_success(), "PreFilter only records the conflict, it does not reject");

    for n in ["n1", "n2"] {
        let status = VolumeRestrictions.filter(&state, &newcomer, snapshot.node(n).unwrap());
        assert!(!status.is_success(), "node {n} must be rejected too — the conflict is not per-node");
        assert!(status.code.is_resolvable_by_preemption(), "evicting the holder does fix this");
        assert!(status.reasons[0].contains("ReadWriteOncePod"));
    }
}

#[test]
fn hypothetically_evicting_the_rwop_holder_admits_the_node() {
    // The whole point of implementing PreFilterExtensions here: preemption's
    // dry run must see a hypothetical eviction reflected in this plugin's
    // state, or the "resolvable by preemption" status above would be a lie.
    let holder = placed_pod("holder", "ns", "n1", "data");
    let snapshot = cluster_with_pvc(rwop_pvc("ns", "data"), &[("n1", holder.clone())]);
    let n1 = snapshot.node("n1").unwrap();

    let mut newcomer = pod("newcomer");
    newcomer.namespace = "ns".to_string();
    newcomer.uid = "uid-newcomer".to_string();
    newcomer.pvc_names = vec!["data".to_string()];

    let mut state = CycleState::default();
    VolumeRestrictions.pre_filter(&mut state, &newcomer, &snapshot);
    assert!(!VolumeRestrictions.filter(&state, &newcomer, n1).is_success());

    let ext = VolumeRestrictions.extensions().expect("VolumeRestrictions implements PreFilterExtensions");
    ext.remove_pod(&mut state, &newcomer, &holder, n1);
    assert!(
        VolumeRestrictions.filter(&state, &newcomer, n1).is_success(),
        "removing the holder must clear the conflict"
    );

    ext.add_pod(&mut state, &newcomer, &holder, n1);
    assert!(
        !VolumeRestrictions.filter(&state, &newcomer, n1).is_success(),
        "putting the holder back must restore the conflict, undoing the dry run"
    );
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
    assert!(status.is_success(), "no current holder, so nothing to reject anywhere");
    assert!(VolumeRestrictions.filter(&state, &p, &node("n1")).is_success());
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
    assert!(!status.is_rejection());
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
fn it_registers_the_upstream_event_contract() {
    let events = VolumeRestrictions.events_to_register();
    let registered: Vec<_> = events.into_iter().map(|e| e.event).collect();
    assert_eq!(
        registered,
        vec![
            ClusterEvent::new(EventResource::AssignedPod, ActionType::DELETE),
            ClusterEvent::new(EventResource::Node, ActionType::ADD),
            ClusterEvent::new(EventResource::PersistentVolumeClaim, ActionType::ADD),
        ]
    );
}
