use super::*;
use k8s_openapi::api::core::v1::PodSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

fn pod(name: &str, priority_class: Option<&str>, deleting: bool) -> Pod {
    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some("default".to_string()),
            deletion_timestamp: deleting.then(|| Time(k8s_openapi::jiff::Timestamp::now())),
            ..Default::default()
        },
        spec: Some(PodSpec {
            priority_class_name: priority_class.map(|s| s.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn ordinary_pods_are_non_critical() {
    let pods = vec![pod("app", None, false)];
    let (non_critical, critical) = split_by_criticality(&pods);
    assert_eq!(non_critical.len(), 1);
    assert_eq!(critical.len(), 0);
}

#[test]
fn system_node_critical_pods_are_critical() {
    let pods = vec![pod("kube-proxy", Some("system-node-critical"), false)];
    let (non_critical, critical) = split_by_criticality(&pods);
    assert_eq!(non_critical.len(), 0);
    assert_eq!(critical.len(), 1);
}

#[test]
fn system_cluster_critical_pods_are_critical() {
    let pods = vec![pod("coredns", Some("system-cluster-critical"), false)];
    let (non_critical, critical) = split_by_criticality(&pods);
    assert_eq!(non_critical.len(), 0);
    assert_eq!(critical.len(), 1);
}

#[test]
fn pods_already_being_deleted_are_excluded_from_both() {
    let pods = vec![pod("app", None, true), pod("kube-proxy", Some("system-node-critical"), true)];
    let (non_critical, critical) = split_by_criticality(&pods);
    assert_eq!(non_critical.len(), 0);
    assert_eq!(critical.len(), 0);
}

#[test]
fn mixed_set_splits_correctly() {
    let pods = vec![
        pod("app-a", None, false),
        pod("app-b", None, false),
        pod("coredns", Some("system-cluster-critical"), false),
        pod("already-going", None, true),
    ];
    let (non_critical, critical) = split_by_criticality(&pods);
    assert_eq!(non_critical.len(), 2);
    assert_eq!(critical.len(), 1);
    assert_eq!(critical[0].metadata.name.as_deref(), Some("coredns"));
}
