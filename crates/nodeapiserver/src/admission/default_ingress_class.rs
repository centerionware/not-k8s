//! `DefaultIngressClass` — a faithful port of real upstream's mutating
//! admission plugin (`plugin/pkg/admission/network/defaultingressclass`):
//! an Ingress with neither `spec.ingressClassName` nor the legacy
//! `kubernetes.io/ingress.class` annotation is assigned the newest
//! `IngressClass` marked with `ingressclass.kubernetes.io/is-default-class:
//! "true"`.
//!
//! The pure decision/mutation is kept separate from the one storage lookup,
//! matching the other storage-backed admission plugins. `server::listener`
//! lists the cluster-scoped `IngressClass` objects and calls [`mutate`].

use serde_json::Value;

pub const INGRESS_CLASS_ANNOTATION: &str = "kubernetes.io/ingress.class";
pub const IS_DEFAULT_INGRESS_CLASS_ANNOTATION: &str = "ingressclass.kubernetes.io/is-default-class";

pub fn applies_to(group: &str, resource: &str, subresource: &str) -> bool {
    group == "networking.k8s.io" && resource == "ingresses" && subresource.is_empty()
}

fn has_class(ingress: &Value) -> bool {
    if ingress
        .pointer("/spec/ingressClassName")
        .is_some_and(|value| !value.is_null())
    {
        return true;
    }
    ingress
        .pointer("/metadata/annotations")
        .and_then(Value::as_object)
        .is_some_and(|annotations| annotations.contains_key(INGRESS_CLASS_ANNOTATION))
}

fn is_default_class(class: &Value) -> bool {
    class
        .pointer("/metadata/annotations")
        .and_then(|annotations| annotations.get(IS_DEFAULT_INGRESS_CLASS_ANNOTATION))
        .and_then(Value::as_str)
        == Some("true")
}

fn creation_timestamp(class: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = class.pointer("/metadata/creationTimestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.with_timezone(&chrono::Utc))
}

fn class_name(class: &Value) -> &str {
    class
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Returns the default class using upstream's newest-timestamp and
/// name-ascending tie-break rules.
pub fn default_class(classes: &[Value]) -> Option<&Value> {
    classes
        .iter()
        .filter(|class| is_default_class(class))
        .max_by(|left, right| {
            creation_timestamp(left)
                .cmp(&creation_timestamp(right))
                .then_with(|| class_name(right).cmp(class_name(left)))
        })
}

/// Applies the selected default class in place. Returns whether the object
/// was changed.
pub fn mutate(ingress: &mut Value, classes: &[Value]) -> bool {
    if has_class(ingress) {
        return false;
    }
    let Some(class_name) = default_class(classes)
        .map(class_name)
        .filter(|name| !name.is_empty())
    else {
        return false;
    };
    let Some(spec) = ingress.as_object_mut().and_then(|object| {
        object
            .entry("spec")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
    }) else {
        return false;
    };
    spec.insert(
        "ingressClassName".to_string(),
        Value::String(class_name.to_string()),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn class(name: &str, is_default: bool, created: &str) -> Value {
        let annotations = if is_default {
            json!({IS_DEFAULT_INGRESS_CLASS_ANNOTATION: "true"})
        } else {
            json!({})
        };
        json!({"metadata": {"name": name, "creationTimestamp": created, "annotations": annotations}})
    }

    #[test]
    fn applies_only_to_v1_ingresses_without_a_subresource() {
        assert!(applies_to("networking.k8s.io", "ingresses", ""));
        assert!(!applies_to("networking.k8s.io", "ingresses", "status"));
        assert!(!applies_to("", "ingresses", ""));
    }

    #[test]
    fn explicit_field_or_legacy_annotation_prevents_defaulting() {
        let classes = vec![class("default", true, "2024-01-01T00:00:00Z")];
        let mut explicit = json!({"spec": {"ingressClassName": "explicit"}});
        assert!(!mutate(&mut explicit, &classes));
        assert_eq!(explicit["spec"]["ingressClassName"], "explicit");

        let mut annotated =
            json!({"metadata": {"annotations": {INGRESS_CLASS_ANNOTATION: "legacy"}}, "spec": {}});
        assert!(!mutate(&mut annotated, &classes));
        assert!(annotated["spec"].get("ingressClassName").is_none());
    }

    #[test]
    fn no_class_is_a_no_op() {
        let mut ingress = json!({"spec": {}});
        assert!(!mutate(
            &mut ingress,
            &[class("ordinary", false, "2024-01-01T00:00:00Z")]
        ));
    }

    #[test]
    fn newest_default_wins_and_equal_timestamps_use_name_ascending() {
        let classes = vec![
            class("zeta", true, "2024-01-01T00:00:00Z"),
            class("newer", true, "2024-06-01T00:00:00Z"),
            class("alpha", true, "2024-01-01T00:00:00Z"),
        ];
        let mut ingress = json!({"spec": {}});
        assert!(mutate(&mut ingress, &classes));
        assert_eq!(ingress["spec"]["ingressClassName"], "newer");

        let tie = vec![
            class("zeta", true, "2024-01-01T00:00:00Z"),
            class("alpha", true, "2024-01-01T00:00:00Z"),
        ];
        let mut ingress = json!({"spec": {}});
        assert!(mutate(&mut ingress, &tie));
        assert_eq!(ingress["spec"]["ingressClassName"], "alpha");
    }
}
