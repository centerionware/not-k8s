//! `PersistentVolumeClaimResize` — the update-time validation from
//! Kubernetes' `plugin/pkg/admission/storage/persistentvolume/resize`.
//!
//! StorageClass lookup is performed by the listener. This module compares
//! Kubernetes resource quantities and applies the plugin's rules to the old
//! and candidate PVC objects.

use crate::scheme::quantity::Quantity;
use serde_json::Value;

pub fn applies_to(
    operation: crate::admission::attributes::Operation,
    group: &str,
    resource: &str,
    subresource: &str,
) -> bool {
    operation == crate::admission::attributes::Operation::Update
        && group.is_empty()
        && resource == "persistentvolumeclaims"
        && subresource.is_empty()
}

fn storage_class_name(pvc: &Value) -> &str {
    pvc.pointer("/metadata/annotations/volume.beta.kubernetes.io~1storage-class")
        .and_then(Value::as_str)
        .or_else(|| {
            pvc.pointer("/spec/storageClassName")
                .and_then(Value::as_str)
        })
        .unwrap_or("")
}

fn requested_storage(pvc: &Value) -> Option<Quantity> {
    pvc.pointer("/spec/resources/requests/storage")
        .and_then(Value::as_str)
        .and_then(|value| Quantity::parse(value).ok())
}

fn storage_class_expansion_allowed(classes: &[Value], name: &str) -> bool {
    classes.iter().any(|class| {
        class.pointer("/metadata/name").and_then(Value::as_str) == Some(name)
            && class
                .pointer("/allowVolumeExpansion")
                .and_then(Value::as_bool)
                == Some(true)
    })
}

/// Validates a candidate PVC update. No error means either the request is
/// not an expansion or the referenced StorageClass permits it.
pub fn validate_resize(new_pvc: &Value, old_pvc: &Value, classes: &[Value]) -> Result<(), String> {
    let Some(new_size) = requested_storage(new_pvc) else {
        return Ok(());
    };
    let old_size = requested_storage(old_pvc).unwrap_or(Quantity::ZERO);
    if new_size <= old_size {
        return Ok(());
    }
    if old_pvc.pointer("/status/phase").and_then(Value::as_str) != Some("Bound") {
        return Err("Only bound persistent volume claims can be expanded".to_string());
    }
    let new_class = storage_class_name(new_pvc);
    let old_class = storage_class_name(old_pvc);
    if new_class.is_empty()
        || new_class != old_class
        || !storage_class_expansion_allowed(classes, new_class)
    {
        return Err("only dynamically provisioned pvc can be resized and the storageclass that provisions the pvc must support resize".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn class(allowed: bool) -> Value {
        json!({"metadata": {"name": "fast"}, "allowVolumeExpansion": allowed})
    }

    fn pvc(size: &str, phase: &str, class_name: &str) -> Value {
        json!({
            "metadata": {"annotations": {"volume.beta.kubernetes.io/storage-class": class_name}},
            "spec": {"storageClassName": class_name, "resources": {"requests": {"storage": size}}},
            "status": {"phase": phase}
        })
    }

    #[test]
    fn applies_only_to_core_pvc_updates() {
        use crate::admission::attributes::Operation;
        assert!(applies_to(
            Operation::Update,
            "",
            "persistentvolumeclaims",
            ""
        ));
        assert!(!applies_to(
            Operation::Create,
            "",
            "persistentvolumeclaims",
            ""
        ));
        assert!(!applies_to(
            Operation::Update,
            "",
            "persistentvolumeclaims",
            "status"
        ));
    }

    #[test]
    fn expansion_of_a_bound_pvc_is_allowed_by_the_storage_class() {
        assert!(validate_resize(
            &pvc("2Gi", "Bound", "fast"),
            &pvc("1Gi", "Bound", "fast"),
            &[class(true)]
        )
        .is_ok());
    }

    #[test]
    fn expansion_of_an_unbound_pvc_is_rejected() {
        assert!(validate_resize(
            &pvc("2Gi", "Pending", "fast"),
            &pvc("1Gi", "Pending", "fast"),
            &[class(true)]
        )
        .is_err());
    }

    #[test]
    fn expansion_is_rejected_when_the_storage_class_does_not_allow_it() {
        assert!(validate_resize(
            &pvc("2Gi", "Bound", "fast"),
            &pvc("1Gi", "Bound", "fast"),
            &[class(false)]
        )
        .is_err());
    }

    #[test]
    fn shrinking_or_an_unchanged_size_is_allowed() {
        assert!(validate_resize(
            &pvc("1Gi", "Bound", "fast"),
            &pvc("2Gi", "Bound", "fast"),
            &[class(false)]
        )
        .is_ok());
    }

    #[test]
    fn changing_the_storage_class_is_rejected() {
        assert!(validate_resize(
            &pvc("2Gi", "Bound", "other"),
            &pvc("1Gi", "Bound", "fast"),
            &[class(true)]
        )
        .is_err());
    }
}
