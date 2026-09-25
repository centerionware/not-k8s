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

    assert!(is_local_host_network_pod(&local_host_network_pod, "node-a"));
    assert!(!is_local_host_network_pod(
        &remote_host_network_pod,
        "node-a"
    ));
    assert!(!is_local_host_network_pod(&local_ordinary_pod, "node-a"));
}
