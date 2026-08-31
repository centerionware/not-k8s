//! `TaintNodesByCondition` — the default-on admission mutation that gives a
//! newly-created Node the upstream `NotReady`/`NoSchedule` taint.
//!
//! This is a pure port of the stable behavior in upstream's
//! `plugin/pkg/admission/nodetaint/admission.go`: it applies only to a core
//! `Node` create, preserves all submitted taints, and is idempotent. The node
//! controller removes the taint after the node reports Ready.

use serde_json::{json, Value};

const NOT_READY_TAINT_KEY: &str = "node.kubernetes.io/not-ready";
const NOT_READY_TAINT_EFFECT: &str = "NoSchedule";

/// Returns whether this plugin handles the request.
pub fn applies_to(
    operation: crate::admission::attributes::Operation,
    group: &str,
    resource: &str,
    subresource: &str,
) -> bool {
    operation == crate::admission::attributes::Operation::Create
        && group.is_empty()
        && resource == "nodes"
        && subresource.is_empty()
}

/// Adds the upstream NotReady taint when the Node does not already have it.
pub fn mutate(node: &mut Value) {
    let Some(object) = node.as_object_mut() else {
        return;
    };
    let spec = object.entry("spec").or_insert_with(|| json!({}));
    let Some(spec) = spec.as_object_mut() else {
        return;
    };
    let taints = spec.entry("taints").or_insert_with(|| json!([]));
    let Some(taints) = taints.as_array_mut() else {
        return;
    };

    if taints.iter().any(|taint| {
        taint.get("key").and_then(Value::as_str) == Some(NOT_READY_TAINT_KEY)
            && taint.get("effect").and_then(Value::as_str) == Some(NOT_READY_TAINT_EFFECT)
    }) {
        return;
    }

    taints.push(json!({
        "key": NOT_READY_TAINT_KEY,
        "effect": NOT_READY_TAINT_EFFECT,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_not_ready_taint_to_a_new_node() {
        let mut node = json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "node"}});

        mutate(&mut node);

        assert_eq!(
            node["spec"]["taints"],
            json!([{
                "key": NOT_READY_TAINT_KEY,
                "effect": NOT_READY_TAINT_EFFECT,
            }])
        );
    }

    #[test]
    fn preserves_existing_taints_and_is_idempotent() {
        let mut node = json!({
            "spec": {"taints": [{"key": "example.com/custom", "effect": "NoExecute"}]}
        });

        mutate(&mut node);
        mutate(&mut node);

        assert_eq!(node["spec"]["taints"].as_array().unwrap().len(), 2);
        assert_eq!(node["spec"]["taints"][0]["key"], "example.com/custom");
    }

    #[test]
    fn does_not_treat_a_different_effect_as_the_not_ready_taint() {
        let mut node = json!({
            "spec": {"taints": [{"key": NOT_READY_TAINT_KEY, "effect": "NoExecute"}]}
        });

        mutate(&mut node);

        assert_eq!(node["spec"]["taints"].as_array().unwrap().len(), 2);
    }
}
