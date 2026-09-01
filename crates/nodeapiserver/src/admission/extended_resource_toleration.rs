//! `ExtendedResourceToleration` — the pure mutating admission plugin from
//! real upstream (`plugin/pkg/admission/extendedresourcetoleration`). For
//! every Pod create/update, it adds an `Exists`/`NoSchedule` toleration for
//! each extended resource requested by an ordinary or init container.
//!
//! The plugin is intentionally pure: whether a Pod requests an extended
//! resource is entirely visible in the submitted object. The upstream
//! plugin is opt-in in kube-apiserver; this crate has not yet exposed an
//! admission-plugin configuration surface, so it is registered with the
//! other built-in mutators until that surface exists.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use super::attributes::Operation;

/// Whether the request is a Pod operation handled by this plugin.
pub fn applies_to(operation: Operation, group: &str, resource: &str, subresource: &str) -> bool {
    matches!(operation, Operation::Create | Operation::Update)
        && group.is_empty()
        && resource == "pods"
        && subresource.is_empty()
}

/// Kubernetes reserves unqualified names and the `kubernetes.io/` namespace
/// for built-in resources. The admission helper's practical distinction is a
/// qualified name outside that built-in namespace.
fn is_extended_resource(name: &str) -> bool {
    name.contains('/') && !name.starts_with("kubernetes.io/")
}

fn requested_resources(container: &Value, resources: &mut BTreeSet<String>) {
    let Some(requests) = container
        .get("resources")
        .and_then(|value| value.get("requests"))
        .and_then(Value::as_object)
    else {
        return;
    };

    resources.extend(
        requests
            .keys()
            .filter(|name| is_extended_resource(name))
            .cloned(),
    );
}

fn toleration_matches(toleration: &Value, resource: &str) -> bool {
    toleration.get("key").and_then(Value::as_str) == Some(resource)
        && toleration.get("operator").and_then(Value::as_str) == Some("Exists")
        && toleration.get("effect").and_then(Value::as_str) == Some("NoSchedule")
}

/// Add one canonical toleration per requested extended resource. Existing
/// matching tolerations are preserved, making the mutation idempotent.
pub fn mutate(pod: &mut Value) -> bool {
    let mut resources = BTreeSet::new();
    for field in ["containers", "initContainers"] {
        if let Some(containers) = pod
            .pointer(&format!("/spec/{field}"))
            .and_then(Value::as_array)
        {
            for container in containers {
                requested_resources(container, &mut resources);
            }
        }
    }

    if resources.is_empty() {
        return false;
    }

    let existing = pod
        .pointer("/spec/tolerations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut mutated = false;
    let Some(spec) = pod
        .as_object_mut()
        .and_then(|object| object.entry("spec").or_insert_with(|| json!({})).as_object_mut())
    else {
        return false;
    };
    let tolerations = spec.entry("tolerations").or_insert_with(|| json!([]));
    let Some(tolerations) = tolerations.as_array_mut() else {
        return false;
    };

    for resource in resources {
        if existing.iter().any(|toleration| toleration_matches(toleration, &resource))
            || tolerations.iter().any(|toleration| toleration_matches(toleration, &resource))
        {
            continue;
        }
        tolerations.push(json!({
            "key": resource,
            "operator": "Exists",
            "effect": "NoSchedule"
        }));
        mutated = true;
    }
    mutated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_only_to_pod_create_and_update() {
        assert!(applies_to(Operation::Create, "", "pods", ""));
        assert!(applies_to(Operation::Update, "", "pods", ""));
        assert!(!applies_to(Operation::Delete, "", "pods", ""));
        assert!(!applies_to(Operation::Create, "", "pods", "status"));
        assert!(!applies_to(Operation::Create, "apps", "pods", ""));
    }

    #[test]
    fn adds_sorted_tolerations_for_regular_and_init_container_requests() {
        let mut pod = json!({
            "spec": {
                "containers": [{
                    "resources": {"requests": {"example.com/gpu": "1", "vendor.io/fpga": "1"}}
                }],
                "initContainers": [{
                    "resources": {"requests": {"example.com/asic": "1"}}
                }]
            }
        });

        assert!(mutate(&mut pod));
        assert_eq!(
            pod["spec"]["tolerations"],
            json!([
                {"key": "example.com/asic", "operator": "Exists", "effect": "NoSchedule"},
                {"key": "example.com/gpu", "operator": "Exists", "effect": "NoSchedule"},
                {"key": "vendor.io/fpga", "operator": "Exists", "effect": "NoSchedule"}
            ])
        );
    }

    #[test]
    fn does_not_duplicate_existing_tolerations_or_treat_builtins_as_extended() {
        let mut pod = json!({
            "spec": {
                "containers": [{
                    "resources": {"requests": {
                        "cpu": "1",
                        "memory": "1Gi",
                        "example.com/gpu": "1"
                    }}
                }],
                "tolerations": [{"key": "example.com/gpu", "operator": "Exists", "effect": "NoSchedule"}]
            }
        });

        assert!(!mutate(&mut pod));
        assert_eq!(pod["spec"]["tolerations"].as_array().unwrap().len(), 1);
    }
}
