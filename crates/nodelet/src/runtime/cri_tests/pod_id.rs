//! pod_id(): extracts the identity nodelet uses to label/find sandboxes.
//! A wrong default here (e.g. losing the real uid) breaks resolve_volumes()'s
//! per-pod directory naming and sandbox_config()'s log_directory, silently.
use super::*;

fn pod_json(v: serde_json::Value) -> Pod {
    serde_json::from_value(v).unwrap()
}

#[test]
fn extracts_namespace_name_and_uid_from_a_normal_pod() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "coredns-abc", "namespace": "kube-system", "uid": "real-uid-1" },
        "spec": { "containers": [] }
    }));
    let id = pod_id(&pod);
    assert_eq!(id.namespace, "kube-system");
    assert_eq!(id.name, "coredns-abc");
    assert_eq!(id.uid, "real-uid-1");
    assert!(!id.host_network);
}

#[test]
fn missing_namespace_defaults_to_default() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "smoke-test", "uid": "u" },
        "spec": { "containers": [] }
    }));
    assert_eq!(pod_id(&pod).namespace, "default");
}

#[test]
fn missing_uid_falls_back_to_namespace_underscore_name() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "x", "namespace": "ns" },
        "spec": { "containers": [] }
    }));
    assert_eq!(pod_id(&pod).uid, "ns_x");
}

#[test]
fn host_network_true_is_read_through() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "x", "namespace": "ns", "uid": "u" },
        "spec": { "hostNetwork": true, "containers": [] }
    }));
    assert!(pod_id(&pod).host_network);
}

#[test]
fn host_network_absent_defaults_false() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "x", "namespace": "ns", "uid": "u" },
        "spec": { "containers": [] }
    }));
    assert!(!pod_id(&pod).host_network);
}

#[test]
fn host_pid_host_ipc_and_share_process_namespace_are_read_through() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "x", "namespace": "ns", "uid": "u" },
        "spec": { "hostPID": true, "hostIPC": true, "shareProcessNamespace": true, "containers": [] }
    }));
    let id = pod_id(&pod);
    assert!(id.host_pid);
    assert!(id.host_ipc);
    assert!(id.share_process_namespace);
}

#[test]
fn host_pid_host_ipc_and_share_process_namespace_absent_default_false() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "x", "namespace": "ns", "uid": "u" },
        "spec": { "containers": [] }
    }));
    let id = pod_id(&pod);
    assert!(!id.host_pid);
    assert!(!id.host_ipc);
    assert!(!id.share_process_namespace);
}
