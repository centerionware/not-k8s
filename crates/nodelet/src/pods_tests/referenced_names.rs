//! referenced_configmap_names()/referenced_secret_names(): which pods need
//! re-materializing when a ConfigMap/Secret changes (Round 37, live-update).
use super::*;

fn pod_json(v: serde_json::Value) -> Pod {
    serde_json::from_value(v).unwrap()
}

#[test]
fn direct_configmap_volume_is_referenced() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "p", "namespace": "ns" },
        "spec": {
            "containers": [{"name": "app", "image": "i"}],
            "volumes": [{"name": "cfg", "configMap": {"name": "my-config"}}]
        }
    }));
    let names = referenced_configmap_names(&pod);
    assert!(names.contains("my-config"));
    assert_eq!(names.len(), 1);
    assert!(referenced_secret_names(&pod).is_empty());
}

#[test]
fn direct_secret_volume_is_referenced() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "p", "namespace": "ns" },
        "spec": {
            "containers": [{"name": "app", "image": "i"}],
            "volumes": [{"name": "sec", "secret": {"secretName": "my-secret"}}]
        }
    }));
    let names = referenced_secret_names(&pod);
    assert!(names.contains("my-secret"));
    assert_eq!(names.len(), 1);
    assert!(referenced_configmap_names(&pod).is_empty());
}

#[test]
fn projected_volume_sources_are_referenced() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "p", "namespace": "ns" },
        "spec": {
            "containers": [{"name": "app", "image": "i"}],
            "volumes": [{
                "name": "proj",
                "projected": {
                    "sources": [
                        {"configMap": {"name": "proj-cfg"}},
                        {"secret": {"name": "proj-sec"}}
                    ]
                }
            }]
        }
    }));
    assert!(referenced_configmap_names(&pod).contains("proj-cfg"));
    assert!(referenced_secret_names(&pod).contains("proj-sec"));
}

#[test]
fn env_only_configmap_reference_is_not_included() {
    // envFrom/valueFrom are captured once at container start and never
    // refreshed by real kubelet either — this must NOT trigger a
    // re-materialization reconcile.
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "p", "namespace": "ns" },
        "spec": {
            "containers": [{
                "name": "app", "image": "i",
                "envFrom": [{"configMapRef": {"name": "env-only-config"}}]
            }]
        }
    }));
    assert!(referenced_configmap_names(&pod).is_empty());
}

#[test]
fn no_volumes_returns_empty() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "p", "namespace": "ns" },
        "spec": { "containers": [{"name": "app", "image": "i"}] }
    }));
    assert!(referenced_configmap_names(&pod).is_empty());
    assert!(referenced_secret_names(&pod).is_empty());
}
