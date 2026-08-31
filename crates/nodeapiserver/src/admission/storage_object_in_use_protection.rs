//! `StorageObjectInUseProtection` — the default upstream mutating admission
//! plugin for PVs, PVCs, and (when enabled upstream) VolumeAttributesClasses.
//! It adds the standard protection finalizer at create time; the corresponding
//! nodecontroller protection controllers remove it once the object is safe to
//! delete.

use serde_json::Value;

pub const PV_PROTECTION_FINALIZER: &str = "kubernetes.io/pv-protection";
pub const PVC_PROTECTION_FINALIZER: &str = "kubernetes.io/pvc-protection";
pub const VAC_PROTECTION_FINALIZER: &str = "kubernetes.io/vac-protection";

fn applies_to(group: &str, resource: &str, subresource: &str) -> Option<&'static str> {
    if !subresource.is_empty() {
        return None;
    }
    match (group, resource) {
        ("", "persistentvolumes") => Some(PV_PROTECTION_FINALIZER),
        ("", "persistentvolumeclaims") => Some(PVC_PROTECTION_FINALIZER),
        ("storage.k8s.io", "volumeattributesclasses") => Some(VAC_PROTECTION_FINALIZER),
        _ => None,
    }
}

/// Adds the upstream protection finalizer when this is a supported resource
/// create. Returns whether the object was changed.
pub fn mutate(group: &str, resource: &str, subresource: &str, object: &mut Value) -> bool {
    let Some(finalizer) = applies_to(group, resource, subresource) else {
        return false;
    };
    let Some(metadata) = object.get_mut("metadata").and_then(Value::as_object_mut) else {
        return false;
    };
    let finalizers = metadata
        .entry("finalizers")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(items) = finalizers.as_array_mut() else {
        return false;
    };
    if items.iter().any(|item| item.as_str() == Some(finalizer)) {
        return false;
    }
    items.push(Value::String(finalizer.to_string()));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adds_the_matching_finalizer_to_supported_creates() {
        let cases = [
            ("", "persistentvolumes", PV_PROTECTION_FINALIZER),
            ("", "persistentvolumeclaims", PVC_PROTECTION_FINALIZER),
            (
                "storage.k8s.io",
                "volumeattributesclasses",
                VAC_PROTECTION_FINALIZER,
            ),
        ];
        for (group, resource, expected) in cases {
            let mut object = json!({"metadata": {}});
            assert!(mutate(group, resource, "", &mut object));
            assert_eq!(object["metadata"]["finalizers"], json!([expected]));
        }
    }

    #[test]
    fn existing_finalizer_is_not_duplicated() {
        let mut pv = json!({"metadata": {"finalizers": [PV_PROTECTION_FINALIZER]}});
        assert!(!mutate("", "persistentvolumes", "", &mut pv));
        assert_eq!(pv["metadata"]["finalizers"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn subresources_and_other_resources_are_ignored() {
        let mut object = json!({"metadata": {}});
        assert!(!mutate("", "persistentvolumes", "status", &mut object));
        assert!(!mutate("", "configmaps", "", &mut object));
        assert!(object["metadata"].get("finalizers").is_none());
    }
}
