use super::*;

#[test]
fn only_local_host_network_pods_bypass_the_coredns_gate() {
    let local_host_network_pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "cilium", "namespace": "kube-system"},
        "spec": {"nodeName": "node-a", "hostNetwork": true}
    }))
    .unwrap();
    let remote_host_network_pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "cilium", "namespace": "kube-system"},
        "spec": {"nodeName": "node-b", "hostNetwork": true}
    }))
    .unwrap();
    let local_ordinary_pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "application", "namespace": "default"},
        "spec": {"nodeName": "node-a", "hostNetwork": false}
    }))
    .unwrap();
    let local_terminating_ordinary_pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {
            "name": "csi-driver",
            "namespace": "default",
            "deletionTimestamp": "2026-09-26T01:55:25Z"
        },
        "spec": {"nodeName": "node-a", "hostNetwork": false}
    }))
    .unwrap();
    let remote_terminating_pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {
            "name": "csi-driver",
            "namespace": "default",
            "deletionTimestamp": "2026-09-26T01:55:25Z"
        },
        "spec": {"nodeName": "node-b", "hostNetwork": false}
    }))
    .unwrap();

    assert!(is_local_host_network_pod(&local_host_network_pod, "node-a"));
    assert!(!is_local_host_network_pod(
        &remote_host_network_pod,
        "node-a"
    ));
    assert!(!is_local_host_network_pod(&local_ordinary_pod, "node-a"));
    assert!(is_local_terminating_pod(
        &local_terminating_ordinary_pod,
        "node-a"
    ));
    assert!(!is_local_terminating_pod(
        &remote_terminating_pod,
        "node-a"
    ));
    assert!(!is_local_terminating_pod(&local_ordinary_pod, "node-a"));
}
