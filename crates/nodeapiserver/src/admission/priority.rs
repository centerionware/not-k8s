//! `Priority` — the Pod mutation and PriorityClass validation from
//! Kubernetes' `plugin/pkg/admission/priority/admission.go`.
//!
//! The listener performs the storage lookups. This module contains the
//! deterministic object rules so they can be tested without a cluster.

use serde_json::{Map, Value};

const DEFAULT_PRIORITY: i64 = 0;
const DEFAULT_PREEMPTION_POLICY: &str = "PreemptLowerPriority";

pub fn applies_to_pod(
    operation: crate::admission::attributes::Operation,
    group: &str,
    resource: &str,
    subresource: &str,
) -> bool {
    matches!(
        operation,
        crate::admission::attributes::Operation::Create
            | crate::admission::attributes::Operation::Update
    ) && group.is_empty()
        && resource == "pods"
        && subresource.is_empty()
}

pub fn applies_to_priority_class(
    operation: crate::admission::attributes::Operation,
    group: &str,
    resource: &str,
    subresource: &str,
) -> bool {
    matches!(
        operation,
        crate::admission::attributes::Operation::Create
            | crate::admission::attributes::Operation::Update
    ) && group == "scheduling.k8s.io"
        && resource == "priorityclasses"
        && subresource.is_empty()
}

fn pod_spec_mut(pod: &mut Value) -> Result<&mut Map<String, Value>, String> {
    pod.as_object_mut()
        .and_then(|object| {
            object
                .entry("spec")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
        })
        .ok_or_else(|| "Pod spec must be an object".to_string())
}

fn class_name(priority_class: &Value) -> &str {
    priority_class
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn class_priority(priority_class: &Value) -> i64 {
    priority_class
        .pointer("/value")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_PRIORITY)
}

fn class_preemption_policy(priority_class: Option<&Value>) -> Option<&str> {
    priority_class
        .and_then(|priority_class| priority_class.pointer("/preemptionPolicy"))
        .and_then(Value::as_str)
        .or(Some(DEFAULT_PREEMPTION_POLICY))
}

/// Mutates a Pod using the named class, or the lowest-valued global default
/// class. `None` means no global default exists.
pub fn mutate_pod(
    pod: &mut Value,
    priority_class: Option<&Value>,
    default_class: Option<&Value>,
) -> Result<(), String> {
    let selected = priority_class.or(default_class);
    let priority = selected.map(class_priority).unwrap_or(DEFAULT_PRIORITY);
    if let Some(provided) = pod.pointer("/spec/priority").and_then(Value::as_i64) {
        if provided != priority {
            return Err(format!(
                "the integer value of priority ({provided}) must not be provided in pod spec; priority admission controller computed {priority}"
            ));
        }
    }
    if let Some(policy) = class_preemption_policy(selected) {
        if let Some(provided) = pod
            .pointer("/spec/preemptionPolicy")
            .and_then(Value::as_str)
        {
            if provided != policy {
                return Err(format!(
                    "the string value of PreemptionPolicy ({provided}) must not be provided in pod spec; priority admission controller computed {policy}"
                ));
            }
        }
    }

    let spec = pod_spec_mut(pod)?;
    if let Some(default_class) = default_class {
        spec.insert(
            "priorityClassName".to_string(),
            Value::String(class_name(default_class).to_string()),
        );
    }
    spec.insert("priority".to_string(), Value::Number(priority.into()));
    if let Some(policy) = class_preemption_policy(selected) {
        spec.insert(
            "preemptionPolicy".to_string(),
            Value::String(policy.to_string()),
        );
    }
    Ok(())
}

/// Preserve fields written by this plugin when an UPDATE body omits them.
pub fn preserve_update_fields(pod: &mut Value, old_pod: &Value) -> Result<(), String> {
    let old_spec = old_pod.get("spec").and_then(Value::as_object);
    let spec = pod_spec_mut(pod)?;
    for field in ["priority", "preemptionPolicy"] {
        if !spec.contains_key(field) {
            if let Some(value) = old_spec.and_then(|old_spec| old_spec.get(field)) {
                spec.insert(field.to_string(), value.clone());
            }
        }
    }
    Ok(())
}

/// Reject a second global default PriorityClass. The existing object is the
/// object being updated, when this is an UPDATE.
pub fn validate_priority_class(object: &Value, existing: &[Value]) -> Option<String> {
    if object.pointer("/globalDefault").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let name = class_name(object);
    existing.iter().find_map(|candidate| {
        let is_default = candidate.pointer("/globalDefault").and_then(Value::as_bool) == Some(true);
        let candidate_name = class_name(candidate);
        if is_default && candidate_name != name {
            Some(format!(
                "PriorityClass {candidate_name} is already marked as default. Only one can exist"
            ))
        } else {
            None
        }
    })
}

/// Select the global default with the lowest priority value. This is the
/// race-safe tie-break used by upstream when multiple defaults exist.
pub fn select_default(classes: &[Value]) -> Option<&Value> {
    classes
        .iter()
        .filter(|class| class.pointer("/globalDefault").and_then(Value::as_bool) == Some(true))
        .min_by(|left, right| {
            class_priority(left)
                .cmp(&class_priority(right))
                .then_with(|| class_name(left).cmp(class_name(right)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn class(name: &str, value: i64, default: bool) -> Value {
        json!({"metadata": {"name": name}, "value": value, "globalDefault": default, "preemptionPolicy": "PreemptLowerPriority"})
    }

    #[test]
    fn applies_only_to_pods_and_priority_classes_on_create_or_update() {
        use crate::admission::attributes::Operation;
        assert!(applies_to_pod(Operation::Create, "", "pods", ""));
        assert!(applies_to_pod(Operation::Update, "", "pods", ""));
        assert!(!applies_to_pod(Operation::Delete, "", "pods", ""));
        assert!(applies_to_priority_class(
            Operation::Create,
            "scheduling.k8s.io",
            "priorityclasses",
            ""
        ));
        assert!(!applies_to_priority_class(
            Operation::Create,
            "",
            "priorityclasses",
            ""
        ));
    }

    #[test]
    fn a_named_class_sets_priority_and_preemption_policy() {
        let mut pod = json!({"spec": {"priorityClassName": "high"}});
        mutate_pod(&mut pod, Some(&class("high", 100, false)), None).unwrap();
        assert_eq!(pod["spec"]["priority"], 100);
        assert_eq!(pod["spec"]["preemptionPolicy"], "PreemptLowerPriority");
    }

    #[test]
    fn the_lowest_valued_global_default_is_selected() {
        let classes = vec![class("high", 100, true), class("low", 10, true)];
        let selected = select_default(&classes).unwrap();
        let mut pod = json!({"spec": {}});
        mutate_pod(&mut pod, None, Some(selected)).unwrap();
        assert_eq!(pod["spec"]["priorityClassName"], "low");
        assert_eq!(pod["spec"]["priority"], 10);
    }

    #[test]
    fn a_mismatched_submitted_priority_is_rejected() {
        let mut pod = json!({"spec": {"priority": 1}});
        assert!(mutate_pod(&mut pod, Some(&class("high", 100, false)), None).is_err());
    }

    #[test]
    fn update_fields_are_preserved_when_omitted() {
        let old = json!({"spec": {"priority": 100, "preemptionPolicy": "Never"}});
        let mut pod = json!({"spec": {}});
        preserve_update_fields(&mut pod, &old).unwrap();
        assert_eq!(pod["spec"]["priority"], 100);
        assert_eq!(pod["spec"]["preemptionPolicy"], "Never");
    }

    #[test]
    fn a_second_global_default_is_rejected() {
        let object = class("new", 10, true);
        let existing = vec![class("old", 1, true)];
        assert!(validate_priority_class(&object, &existing).is_some());
    }
}
