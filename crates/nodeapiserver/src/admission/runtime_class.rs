//! `RuntimeClass` — the create-time Pod mutation and validation from
//! Kubernetes' `plugin/pkg/admission/runtimeclass/admission.go`.
//!
//! The listener performs the live cluster lookup. This module keeps the
//! object transformation pure and testable: a named RuntimeClass supplies
//! Pod overhead, merges its scheduling node selector and tolerations, and a
//! Pod may not provide overhead that disagrees with (or has no corresponding)
//! RuntimeClass definition.

use crate::scheme::quantity::Quantity;
use serde_json::{json, Map, Value};

/// RuntimeClass admission only handles ordinary Pod creates.
pub fn applies_to(
    operation: crate::admission::attributes::Operation,
    group: &str,
    resource: &str,
    subresource: &str,
) -> bool {
    operation == crate::admission::attributes::Operation::Create
        && group.is_empty()
        && resource == "pods"
        && subresource.is_empty()
}

fn pod_spec(pod: &Value) -> Option<&Map<String, Value>> {
    pod.get("spec")?.as_object()
}

fn pod_spec_mut(pod: &mut Value) -> Result<&mut Map<String, Value>, String> {
    pod.as_object_mut()
        .and_then(|object| {
            object
                .entry("spec")
                .or_insert_with(|| json!({}))
                .as_object_mut()
        })
        .ok_or_else(|| "Pod spec must be an object".to_string())
}

fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// Resource quantities compare by their Kubernetes quantity value, so
/// equivalent spellings such as `1` and `1000m` do not produce a false
/// RuntimeClass overhead mismatch. Invalid values fall back to exact JSON
/// comparison; normal schema validation reports those separately.
fn resource_lists_equal(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (present(left), present(right)) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
                return left == right;
            };
            left.len() == right.len()
                && left.iter().all(|(key, left_value)| {
                    let Some(right_value) = right.get(key) else {
                        return false;
                    };
                    match (left_value.as_str(), right_value.as_str()) {
                        (Some(left), Some(right)) => {
                            match (Quantity::parse(left), Quantity::parse(right)) {
                                (Ok(left), Ok(right)) => left == right,
                                _ => left == right,
                            }
                        }
                        _ => left_value == right_value,
                    }
                })
        }
        _ => false,
    }
}

fn runtime_class_overhead(runtime_class: Option<&Value>) -> Option<&Value> {
    runtime_class
        .and_then(|class| class.pointer("/overhead/podFixed"))
        .filter(|overhead| !overhead.is_null())
}

fn merge_scheduling(pod: &mut Value, runtime_class: &Value) -> Result<(), String> {
    let Some(scheduling) = runtime_class.pointer("/scheduling") else {
        return Ok(());
    };

    let runtime_selector = scheduling.get("nodeSelector").and_then(Value::as_object);
    let runtime_tolerations = scheduling.get("tolerations").and_then(Value::as_array);
    if runtime_selector.is_none() && runtime_tolerations.is_none() {
        return Ok(());
    }

    let spec = pod_spec_mut(pod)?;
    if let Some(runtime_selector) = runtime_selector {
        let mut merged = spec
            .get("nodeSelector")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (key, runtime_value) in runtime_selector {
            if let Some(pod_value) = merged.get(key) {
                if pod_value != runtime_value {
                    return Err(format!(
                        "conflict: runtimeClass.scheduling.nodeSelector[{key}] = {runtime_value}; pod.spec.nodeSelector[{key}] = {pod_value}"
                    ));
                }
            }
            merged.insert(key.clone(), runtime_value.clone());
        }
        spec.insert("nodeSelector".to_string(), Value::Object(merged));
    }

    if let Some(runtime_tolerations) = runtime_tolerations {
        let tolerations = spec
            .entry("tolerations")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| "Pod spec.tolerations must be an array".to_string())?;
        for runtime_toleration in runtime_tolerations {
            if !tolerations
                .iter()
                .any(|pod_toleration| pod_toleration == runtime_toleration)
            {
                tolerations.push(runtime_toleration.clone());
            }
        }
    }
    Ok(())
}

/// Applies and validates the RuntimeClass contract to a Pod.
///
/// `runtime_class` is `None` when the Pod has no `runtimeClassName`; the
/// listener separately rejects a named class that cannot be found.
pub fn mutate_and_validate(pod: &mut Value, runtime_class: Option<&Value>) -> Result<(), String> {
    let pod_overhead = pod_spec(pod).and_then(|spec| present(spec.get("overhead")));
    let class_overhead = runtime_class_overhead(runtime_class);
    if let Some(class_overhead) = class_overhead {
        let pod_supplied_nonempty_overhead = pod_overhead
            .and_then(Value::as_object)
            .map_or(true, |overhead| !overhead.is_empty());
        if pod_overhead.is_some()
            && pod_supplied_nonempty_overhead
            && !resource_lists_equal(pod_overhead, Some(class_overhead))
        {
            return Err(
                "pod rejected: Pod's Overhead doesn't match RuntimeClass's defined Overhead"
                    .to_string(),
            );
        }
        pod_spec_mut(pod)?.insert("overhead".to_string(), class_overhead.clone());
    } else if pod_overhead.is_some() {
        return Err(
            "pod rejected: Pod Overhead set without corresponding RuntimeClass defined Overhead"
                .to_string(),
        );
    }
    if let Some(runtime_class) = runtime_class {
        merge_scheduling(pod, runtime_class)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_class() -> Value {
        json!({
            "overhead": {"podFixed": {"cpu": "1m", "memory": "16Mi"}},
            "scheduling": {
                "nodeSelector": {"runtime": "sandbox"},
                "tolerations": [{"key": "runtime", "operator": "Exists", "effect": "NoSchedule"}]
            }
        })
    }

    #[test]
    fn applies_only_to_core_pod_creates() {
        use crate::admission::attributes::Operation;
        assert!(applies_to(Operation::Create, "", "pods", ""));
        assert!(!applies_to(Operation::Update, "", "pods", ""));
        assert!(!applies_to(Operation::Create, "", "pods", "status"));
        assert!(!applies_to(Operation::Create, "apps", "pods", ""));
    }

    #[test]
    fn overhead_and_scheduling_are_merged() {
        let mut pod = json!({"spec": {"nodeSelector": {"existing": "yes"}, "tolerations": []}});
        mutate_and_validate(&mut pod, Some(&runtime_class())).unwrap();
        assert_eq!(pod["spec"]["overhead"]["cpu"], "1m");
        assert_eq!(pod["spec"]["nodeSelector"]["existing"], "yes");
        assert_eq!(pod["spec"]["nodeSelector"]["runtime"], "sandbox");
        assert_eq!(pod["spec"]["tolerations"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn equivalent_quantity_spellings_are_accepted() {
        let mut pod = json!({"spec": {"overhead": {"cpu": "0.001", "memory": "16777216"}}});
        mutate_and_validate(&mut pod, Some(&runtime_class())).unwrap();
    }

    #[test]
    fn conflicting_node_selector_is_rejected() {
        let mut pod = json!({"spec": {"nodeSelector": {"runtime": "runc"}}});
        let error = mutate_and_validate(&mut pod, Some(&runtime_class())).unwrap_err();
        assert!(error.contains("nodeSelector[runtime]"));
    }

    #[test]
    fn pod_overhead_without_a_runtime_class_is_rejected() {
        let mut pod = json!({"spec": {"overhead": {"cpu": "1m"}}});
        assert!(mutate_and_validate(&mut pod, None).is_err());
    }

    #[test]
    fn duplicate_runtime_tolerations_are_not_added() {
        let class = runtime_class();
        let mut pod = json!({"spec": {"tolerations": [{"key": "runtime", "operator": "Exists", "effect": "NoSchedule"} ]}});
        mutate_and_validate(&mut pod, Some(&class)).unwrap();
        assert_eq!(pod["spec"]["tolerations"].as_array().unwrap().len(), 1);
    }
}
