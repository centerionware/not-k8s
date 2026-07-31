//! ephemeral_pvc_name()/pvc_owned_by_pod(): the pure logic behind generic
//! ephemeral volumes (round 31) — the deterministic PVC naming
//! convention, and the ownership safety check real kubelet itself does
//! before trusting a same-named PVC.
use super::*;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

#[test]
fn pvc_name_concatenates_pod_and_volume_name_with_a_dash() {
    assert_eq!(ephemeral_pvc_name("myapp", "scratch"), "myapp-scratch");
}

fn pvc_with_owners(owners: Vec<OwnerReference>) -> PersistentVolumeClaim {
    PersistentVolumeClaim { metadata: ObjectMeta { owner_references: Some(owners), ..Default::default() }, ..Default::default() }
}

fn owner(uid: &str) -> OwnerReference {
    OwnerReference {
        api_version: "v1".to_string(),
        kind: "Pod".to_string(),
        name: "myapp".to_string(),
        uid: uid.to_string(),
        controller: Some(true),
        ..Default::default()
    }
}

#[test]
fn owned_by_the_matching_pod_uid_is_trusted() {
    let pvc = pvc_with_owners(vec![owner("pod-uid-1")]);
    assert!(pvc_owned_by_pod(&pvc, "pod-uid-1"));
}

#[test]
fn no_owner_references_at_all_is_not_trusted() {
    let pvc = PersistentVolumeClaim::default();
    assert!(!pvc_owned_by_pod(&pvc, "pod-uid-1"));
}

#[test]
fn owned_by_a_different_uid_is_not_trusted() {
    // Same name, different UID — e.g. a leftover PVC from a previous pod
    // that happened to get the same deterministic name.
    let pvc = pvc_with_owners(vec![owner("some-other-uid")]);
    assert!(!pvc_owned_by_pod(&pvc, "pod-uid-1"));
}

#[test]
fn one_matching_owner_among_several_is_still_trusted() {
    let pvc = pvc_with_owners(vec![owner("unrelated-uid"), owner("pod-uid-1")]);
    assert!(pvc_owned_by_pod(&pvc, "pod-uid-1"));
}
