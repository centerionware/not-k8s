//! Structural-schema pruning for built-in and CRD objects: drops any object
//! key a CRD's own `openAPIV3Schema` doesn't declare, unless that
//! schema (at that level, or any ancestor level) sets
//! `x-kubernetes-preserve-unknown-fields: true` — a faithful, if
//! scoped-down, port of real upstream's own recursive walk
//! (`k8s.io/apiextensions-apiserver/pkg/apiserver/schema/pruning`).
//! Real upstream default posture for `apiextensions.k8s.io/v1` (unlike
//! the deprecated `v1beta1`, which allowed unstructured schemas): a
//! structural schema prunes by default, an operator opts a subtree back
//! into "anything goes" explicitly via `x-kubernetes-preserve-unknown-
//! fields`, never the reverse.
//!
//! Run *before* defaulting and required/type validation
//! (`server::rest::create`/`update`/`patch_persist`'s own call order),
//! matching real upstream's own sequencing: a default the schema itself
//! names is, by definition, a declared field, so pruning never removes
//! one; validation then sees the object as it will actually be stored,
//! not one still carrying fields about to be dropped.
//!
//! **Named, honest simplification**: `apiVersion`/`kind`/`metadata` are
//! hard-coded as always preserved at the object's own top level,
//! standing in for real upstream's schema-*completion* step
//! (`pkg/apiserver/schema`'s structural-schema normalization), which
//! auto-injects those three into a CRD's own effective schema
//! regardless of what the operator wrote — this module doesn't
//! implement that general completion mechanism, only this one specific,
//! well-known, security-relevant consequence of it (an operator whose
//! schema only ever describes `spec`/`status`, the overwhelming common
//! case, must never have this build silently prune the object's own
//! identity).
//!
//! OpenAPI `allOf` branches contribute to the object's effective properties.
//! This matters for upstream built-in schemas, which use `allOf` to represent
//! embedded Go fields. Treat their property declarations as one combined
//! object while pruning, so fields are not lost merely because they are
//! nested in an `allOf` branch.

use serde_json::Value;

const ALWAYS_PRESERVED_TOP_LEVEL_FIELDS: &[&str] = &["apiVersion", "kind", "metadata"];

fn preserves_unknown_fields(schema: &Value) -> bool {
    schema.get("x-kubernetes-preserve-unknown-fields").and_then(Value::as_bool) == Some(true)
}

/// Collect properties across an OpenAPI `allOf` composition. Kubernetes' own
/// generated built-in schemas use `allOf` for embedded Go fields; treating
/// only the outer object's `properties` as declared would prune those fields
/// from ordinary API writes.
fn property_schemas<'a>(schema: &'a Value, name: &str, out: &mut Vec<&'a Value>) {
    if let Some(property) = schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(name))
    {
        out.push(property);
    }
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for branch in all_of {
            property_schemas(branch, name, out);
        }
    }
}

fn additional_property_schemas<'a>(schema: &'a Value, out: &mut Vec<&'a Value>) {
    if let Some(additional) = schema.get("additionalProperties") {
        out.push(additional);
    }
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for branch in all_of {
            additional_property_schemas(branch, out);
        }
    }
}

/// Returns a pruned copy of `value` per `schema` — `value` itself is
/// never mutated, same convention `apiextensions::schema_defaults::
/// apply_defaults` uses.
pub fn prune(schema: &Value, value: &Value) -> Value {
    let mut out = value.clone();
    prune_in_place(schema, &mut out, true);
    out
}

fn prune_in_place(schema: &Value, value: &mut Value, is_root: bool) {
    if preserves_unknown_fields(schema) {
        return;
    }

    if let Some(obj) = value.as_object_mut() {
        let keys: Vec<String> = obj.keys().cloned().collect();
        for key in keys {
            if is_root && ALWAYS_PRESERVED_TOP_LEVEL_FIELDS.contains(&key.as_str()) {
                continue;
            }
            let mut property_matches = Vec::new();
            property_schemas(schema, &key, &mut property_matches);
            if !property_matches.is_empty() {
                if let Some(child) = obj.get_mut(&key) {
                    for property_schema in property_matches {
                        prune_in_place(property_schema, child, false);
                    }
                }
                continue;
            }

            let mut additional_matches = Vec::new();
            additional_property_schemas(schema, &mut additional_matches);
            let additional_schemas: Vec<_> = additional_matches
                .iter()
                .copied()
                .filter(|additional| additional.is_object())
                .collect();
            if !additional_schemas.is_empty() {
                if let Some(child) = obj.get_mut(&key) {
                    for additional_schema in additional_schemas {
                        prune_in_place(additional_schema, child, false);
                    }
                }
            } else if !additional_matches
                .iter()
                .any(|additional| additional.as_bool() == Some(true))
            {
                obj.remove(&key);
            }
        }
        return;
    }

    if let Some(arr) = value.as_array_mut() {
        if let Some(items_schema) = schema.get("items") {
            for item in arr.iter_mut() {
                prune_in_place(items_schema, item, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_undeclared_field_is_dropped() {
        let schema = json!({"type": "object", "properties": {"color": {"type": "string"}}});
        let value = json!({"color": "red", "totallyUnknown": "gone"});
        assert_eq!(prune(&schema, &value), json!({"color": "red"}));
    }

    #[test]
    fn a_declared_field_survives() {
        let schema = json!({"type": "object", "properties": {"color": {"type": "string"}}});
        let value = json!({"color": "red"});
        assert_eq!(prune(&schema, &value), value);
    }

    #[test]
    fn pruning_recurses_into_nested_objects() {
        let schema = json!({"type": "object", "properties": {"spec": {"type": "object", "properties": {"color": {"type": "string"}}}}});
        let value = json!({"spec": {"color": "red", "junk": 1}});
        assert_eq!(prune(&schema, &value), json!({"spec": {"color": "red"}}));
    }

    #[test]
    fn all_of_embedded_properties_survive_and_are_pruned_together() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "allOf": [
                        {"type": "object", "properties": {"first": {"type": "string"}}},
                        {"type": "object", "properties": {"second": {"type": "boolean"}}}
                    ]
                }
            }
        });
        let value = json!({"spec": {"first": "kept", "second": true, "unknown": "dropped"}});
        assert_eq!(
            prune(&schema, &value),
            json!({"spec": {"first": "kept", "second": true}})
        );
    }

    #[test]
    fn pruning_recurses_into_array_items() {
        let schema = json!({"type": "object", "properties": {"ports": {"type": "array", "items": {"type": "object", "properties": {"number": {"type": "integer"}}}}}});
        let value = json!({"ports": [{"number": 80, "junk": true}]});
        assert_eq!(prune(&schema, &value), json!({"ports": [{"number": 80}]}));
    }

    #[test]
    fn a_schema_shaped_additional_properties_keeps_the_key_but_still_prunes_its_value() {
        let schema = json!({"type": "object", "properties": {"labels": {"type": "object", "additionalProperties": {"type": "object", "properties": {"weight": {"type": "integer"}}}}}});
        let value = json!({"labels": {"a": {"weight": 1, "junk": "x"}}});
        assert_eq!(prune(&schema, &value), json!({"labels": {"a": {"weight": 1}}}));
    }

    #[test]
    fn a_bare_boolean_additional_properties_true_preserves_everything_under_it() {
        let schema = json!({"type": "object", "properties": {"data": {"type": "object", "additionalProperties": true}}});
        let value = json!({"data": {"whatever": "shape", "another": 5}});
        assert_eq!(prune(&schema, &value), value);
    }

    #[test]
    fn x_kubernetes_preserve_unknown_fields_stops_pruning_at_that_level() {
        let schema = json!({"type": "object", "properties": {"spec": {"type": "object", "x-kubernetes-preserve-unknown-fields": true, "properties": {"color": {"type": "string"}}}}});
        let value = json!({"spec": {"color": "red", "junk": {"nested": true}}});
        assert_eq!(prune(&schema, &value), value, "everything under the preserving level must survive untouched");
    }

    #[test]
    fn api_version_kind_and_metadata_survive_at_the_top_level_even_when_the_schema_never_mentions_them() {
        // The overwhelmingly common real case: an operator's schema only
        // ever describes spec/status.
        let schema = json!({"type": "object", "properties": {"spec": {"type": "object", "properties": {"color": {"type": "string"}}}}});
        let value = json!({
            "apiVersion": "example.com/v1",
            "kind": "Widget",
            "metadata": {"name": "w1", "someFutureField": "x"},
            "spec": {"color": "red", "junk": 1},
        });
        let pruned = prune(&schema, &value);
        assert_eq!(pruned["apiVersion"], "example.com/v1");
        assert_eq!(pruned["kind"], "Widget");
        assert_eq!(pruned["metadata"], json!({"name": "w1", "someFutureField": "x"}), "metadata is never pruned, even fields the CRD schema doesn't itself describe");
        assert_eq!(pruned["spec"], json!({"color": "red"}));
    }

    #[test]
    fn a_field_named_like_a_preserved_top_level_field_is_not_special_cased_when_nested() {
        // The apiVersion/kind/metadata exemption is root-only -- a
        // *nested* field that happens to share one of those names must
        // still be pruned normally if undeclared.
        let schema = json!({"type": "object", "properties": {"spec": {"type": "object", "properties": {"color": {"type": "string"}}}}});
        let value = json!({"spec": {"color": "red", "kind": "not-special-here"}});
        assert_eq!(prune(&schema, &value), json!({"spec": {"color": "red"}}));
    }
}
