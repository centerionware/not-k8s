//! Kubernetes `meta.k8s.io/v1` PartialObjectMetadata conversion.
//!
//! The representation intentionally keeps the complete `metadata` object and
//! discards the resource-specific fields. This is the standard shape used by
//! clients that need labels, ownership, resource versions, or timestamps but
//! do not need to decode a resource's full schema.

use serde_json::{json, Value};

/// Convert one Kubernetes object to `PartialObjectMetadata`.
pub fn object(value: &Value) -> Value {
    json!({
        "apiVersion": "meta.k8s.io/v1",
        "kind": "PartialObjectMetadata",
        "metadata": value.get("metadata").cloned().unwrap_or_else(|| json!({})),
    })
}

/// Convert a Kubernetes List to `PartialObjectMetadataList`.
pub fn list(value: &Value) -> Value {
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(object).collect::<Vec<_>>())
        .unwrap_or_default();
    json!({
        "apiVersion": "meta.k8s.io/v1",
        "kind": "PartialObjectMetadataList",
        "metadata": value.get("metadata").cloned().unwrap_or_else(|| json!({})),
        "items": items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_keeps_metadata_and_drops_resource_fields() {
        let partial = object(&json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "demo", "labels": {"app": "demo"}},
            "spec": {"containers": [{"name": "app"}]},
        }));
        assert_eq!(partial["kind"], "PartialObjectMetadata");
        assert_eq!(partial["metadata"]["name"], "demo");
        assert!(partial.get("spec").is_none());
    }

    #[test]
    fn list_converts_every_item_and_keeps_list_metadata() {
        let partial = list(&json!({
            "kind": "PodList",
            "metadata": {"resourceVersion": "7", "continue": "next"},
            "items": [
                {"metadata": {"name": "one"}, "spec": {}},
                {"metadata": {"name": "two"}, "status": {}},
            ],
        }));
        assert_eq!(partial["kind"], "PartialObjectMetadataList");
        assert_eq!(partial["metadata"]["resourceVersion"], "7");
        assert_eq!(partial["items"][0]["metadata"]["name"], "one");
        assert!(partial["items"][0].get("spec").is_none());
    }
}
