//! key_parts(): namespace/name extraction used to key every write_status call.
use super::*;

fn pod_json(v: serde_json::Value) -> Pod {
    serde_json::from_value(v).unwrap()
}

#[test]
fn extracts_namespace_and_name() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "coredns-abc", "namespace": "kube-system" }
    }));
    assert_eq!(key_parts(&pod), Some(("kube-system".to_string(), "coredns-abc".to_string())));
}

#[test]
fn missing_namespace_defaults_to_default() {
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "smoke-test" }
    }));
    assert_eq!(key_parts(&pod), Some(("default".to_string(), "smoke-test".to_string())));
}

#[test]
fn missing_name_returns_none() {
    // A Pod object without a name shouldn't happen in practice, but
    // reconcile()/teardown() both bail out via this rather than panicking
    // on an empty string used as an API path segment.
    let pod = pod_json(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "namespace": "ns" }
    }));
    assert_eq!(key_parts(&pod), None);
}
