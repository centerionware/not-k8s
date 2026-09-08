//! `DefaultStorageClass` — a faithful port of real upstream's own
//! mutating admission plugin
//! (`plugin/pkg/admission/storage/storageclass/setdefault/admission.go`,
//! release-1.34, fetched and read directly): a `PersistentVolumeClaim`
//! `CREATE` that doesn't already specify a storage class gets
//! `spec.storageClassName` set to whichever `StorageClass` is marked
//! default, if any.
//!
//! "Doesn't already specify a class" is upstream's own real
//! `helper.PersistentVolumeClaimHasClass` (`pkg/apis/core/helper/helpers.go`),
//! ported exactly: the beta `volume.beta.kubernetes.io/storage-class`
//! annotation counts too, not just a non-null `spec.storageClassName` —
//! both checked, either one is enough to skip defaulting.
//!
//! "Whichever `StorageClass` is marked default" is upstream's own real
//! `util.GetDefaultClass`/`IsDefaultAnnotation`
//! (`pkg/volume/util/storageclass.go`), ported exactly: a class counts if
//! either `storageclass.kubernetes.io/is-default-class` or the beta
//! `storageclass.beta.kubernetes.io/is-default-class` annotation is the
//! literal string `"true"`. With more than one default class, upstream
//! picks the newest by `creationTimestamp`, tie-broken by name ascending
//! — ported exactly, including the tie-break (real upstream's own comment:
//! "Primary sort by creation timestamp, newest first / Secondary sort by
//! class name, ascending order").
//!
//! Same split as every other Group J plugin so far: [`applies_to`]/
//! [`mutate`] are pure and unit tested with no I/O; `server::listener`
//! performs the one real I/O step (`server::rest::list` on
//! `storage.k8s.io/v1` `storageclasses`) in between.

use serde_json::Value;

const BETA_STORAGE_CLASS_ANNOTATION: &str = "volume.beta.kubernetes.io/storage-class";
const IS_DEFAULT_STORAGE_CLASS_ANNOTATION: &str = "storageclass.kubernetes.io/is-default-class";
const BETA_IS_DEFAULT_STORAGE_CLASS_ANNOTATION: &str = "storageclass.beta.kubernetes.io/is-default-class";

pub fn applies_to(group: &str, resource: &str, subresource: &str) -> bool {
    group.is_empty() && resource == "persistentvolumeclaims" && subresource.is_empty()
}

fn has_class(pvc: &Value) -> bool {
    let has_beta_annotation = pvc.get("metadata").and_then(|m| m.get("annotations")).and_then(|a| a.get(BETA_STORAGE_CLASS_ANNOTATION)).is_some();
    if has_beta_annotation {
        return true;
    }
    pvc.get("spec").and_then(|s| s.get("storageClassName")).and_then(Value::as_str).is_some()
}

fn is_default_annotation(class: &Value) -> bool {
    let annotations = class.get("metadata").and_then(|m| m.get("annotations"));
    let has = |key: &str| annotations.and_then(|a| a.get(key)).and_then(Value::as_str) == Some("true");
    has(IS_DEFAULT_STORAGE_CLASS_ANNOTATION) || has(BETA_IS_DEFAULT_STORAGE_CLASS_ANNOTATION)
}

fn creation_timestamp(class: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = class.get("metadata")?.get("creationTimestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(|dt| dt.with_timezone(&chrono::Utc))
}

fn class_name(class: &Value) -> &str {
    class.get("metadata").and_then(|m| m.get("name")).and_then(Value::as_str).unwrap_or("")
}

/// Real upstream's own `GetDefaultClass`: every class in `classes`
/// carrying a default annotation, newest `creationTimestamp` first, name
/// ascending as the tie-break — `None` if no class in the list is marked
/// default, matching upstream's own "no default class selected, do
/// nothing" no-op.
fn default_class(classes: &[Value]) -> Option<&Value> {
    classes
        .iter()
        .filter(|c| is_default_annotation(c))
        .max_by(|a, b| creation_timestamp(a).cmp(&creation_timestamp(b)).then_with(|| class_name(b).cmp(class_name(a))))
}

/// Mutates `pvc` in place with the default class's name, if `pvc` has no
/// class of its own and a default exists among `classes`. Returns whether
/// anything was set (observability/tests only).
pub fn mutate(pvc: &mut Value, classes: &[Value]) -> bool {
    if has_class(pvc) {
        return false;
    }
    let Some(default) = default_class(classes) else {
        return false;
    };
    let name = class_name(default).to_string();
    if let Some(spec) = pvc.as_object_mut().and_then(|o| o.entry("spec").or_insert_with(|| serde_json::json!({})).as_object_mut()) {
        spec.insert("storageClassName".to_string(), Value::String(name));
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn class(name: &str, is_default: bool, created: &str) -> Value {
        let mut annotations = json!({});
        if is_default {
            annotations["storageclass.kubernetes.io/is-default-class"] = json!("true");
        }
        json!({"metadata": {"name": name, "creationTimestamp": created, "annotations": annotations}})
    }

    #[test]
    fn applies_only_to_core_pvcs_with_no_subresource() {
        assert!(applies_to("", "persistentvolumeclaims", ""));
        assert!(!applies_to("", "persistentvolumeclaims", "status"));
        assert!(!applies_to("storage.k8s.io", "persistentvolumeclaims", ""));
    }

    #[test]
    fn a_pvc_with_an_explicit_class_is_left_alone() {
        let mut pvc = json!({"spec": {"storageClassName": "fast"}});
        let classes = vec![class("standard", true, "2024-01-01T00:00:00Z")];
        assert!(!mutate(&mut pvc, &classes));
        assert_eq!(pvc["spec"]["storageClassName"], "fast");
    }

    #[test]
    fn a_pvc_with_the_beta_annotation_is_left_alone_even_with_no_spec_class() {
        let mut pvc = json!({"metadata": {"annotations": {"volume.beta.kubernetes.io/storage-class": "fast"}}, "spec": {}});
        let classes = vec![class("standard", true, "2024-01-01T00:00:00Z")];
        assert!(!mutate(&mut pvc, &classes));
        assert!(pvc["spec"].get("storageClassName").is_none());
    }

    #[test]
    fn a_pvc_with_no_class_gets_the_default_class_name() {
        let mut pvc = json!({"spec": {}});
        let classes = vec![class("other", false, "2024-01-01T00:00:00Z"), class("standard", true, "2024-01-02T00:00:00Z")];
        assert!(mutate(&mut pvc, &classes));
        assert_eq!(pvc["spec"]["storageClassName"], "standard");
    }

    #[test]
    fn no_default_class_present_is_a_correct_no_op() {
        let mut pvc = json!({"spec": {}});
        let classes = vec![class("standard", false, "2024-01-01T00:00:00Z")];
        assert!(!mutate(&mut pvc, &classes));
        assert!(pvc["spec"].get("storageClassName").is_none());
    }

    #[test]
    fn multiple_defaults_pick_the_newest_by_creation_timestamp() {
        let mut pvc = json!({"spec": {}});
        let classes = vec![class("older", true, "2024-01-01T00:00:00Z"), class("newer", true, "2024-06-01T00:00:00Z")];
        assert!(mutate(&mut pvc, &classes));
        assert_eq!(pvc["spec"]["storageClassName"], "newer");
    }

    #[test]
    fn a_tie_in_creation_timestamp_is_broken_by_name_ascending() {
        let mut pvc = json!({"spec": {}});
        let classes = vec![class("zeta", true, "2024-01-01T00:00:00Z"), class("alpha", true, "2024-01-01T00:00:00Z")];
        assert!(mutate(&mut pvc, &classes));
        assert_eq!(pvc["spec"]["storageClassName"], "alpha");
    }

    #[test]
    fn the_beta_default_annotation_counts_too() {
        let mut pvc = json!({"spec": {}});
        let classes = vec![json!({"metadata": {"name": "beta-default", "creationTimestamp": "2024-01-01T00:00:00Z", "annotations": {"storageclass.beta.kubernetes.io/is-default-class": "true"}}})];
        assert!(mutate(&mut pvc, &classes));
        assert_eq!(pvc["spec"]["storageClassName"], "beta-default");
    }
}
