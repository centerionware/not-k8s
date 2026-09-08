//! Structural-schema defaulting for CRD-defined objects — the dynamic,
//! runtime-schema counterpart to `scheme::defaulting::apply_defaults`
//! (which walks this crate's own *compiled* `FIELD_META`/`ref_schema`
//! tables for built-in types). A CRD's schema isn't known until an
//! operator submits it, so there's nothing to generate at build time —
//! this module walks whatever `spec.versions[].schema.openAPIV3Schema`
//! JSON tree `apiextensions::registry::resolve` handed back instead.
//!
//! A faithful (if scoped-down) port of real upstream's own recursive
//! walk (`k8s.io/apiextensions-apiserver/pkg/apiserver/schema/
//! defaulting`'s `Default`): for each position, an explicit `null` or a
//! genuinely absent object key gets replaced with a deep copy of that
//! position's own `default` (if the schema names one) and nothing more —
//! real upstream does **not** recursively default *into* a value it just
//! applied a default to, since an operator-authored default is already a
//! complete value for that position. Otherwise, recurse: object
//! properties named in `properties` (plus, for keys the CRD didn't name
//! explicitly, `additionalProperties` when it's itself a schema, not a
//! bare `true`/`false`), and every element of an array against `items`.
//!
//! **Named, honest scope narrowing**: no `x-kubernetes-preserve-unknown-
//! fields` pruning (a genuinely separate structural-schema concern —
//! "should this field even exist" rather than "what does this field
//! default to"), and no full type/required validation against the
//! schema at all yet — see `apiextensions::registry::CrdResource`'s own
//! doc comment and `docs/APISERVER.md`'s Group K section.

use serde_json::Value;

/// Returns a defaulted copy of `value` per `schema` — `value` itself is
/// never mutated (`server::rest`'s own call sites already work with an
/// owned candidate object by this point, same convention
/// `scheme::defaulting::apply_defaults` uses).
pub fn apply_defaults(schema: &Value, value: &Value) -> Value {
    let mut out = value.clone();
    default_in_place(schema, &mut out);
    out
}

fn default_in_place(schema: &Value, value: &mut Value) {
    if value.is_null() {
        if let Some(default) = schema.get("default") {
            *value = default.clone();
        }
        return;
    }

    if value.is_object() {
        let known_properties = schema.get("properties").and_then(Value::as_object);
        if let Some(properties) = known_properties {
            for (prop_name, prop_schema) in properties {
                let Some(obj) = value.as_object_mut() else { break };
                match obj.get_mut(prop_name) {
                    Some(child) => default_in_place(prop_schema, child),
                    None => {
                        if let Some(default) = prop_schema.get("default") {
                            obj.insert(prop_name.clone(), default.clone());
                        }
                    }
                }
            }
        }
        // `additionalProperties` is either a nested schema (apply it to
        // every key this object has that `properties` didn't already
        // cover -- a real "map of X" CRD field) or a bare `true`/`false`
        // (no per-value schema to default against, real upstream's own
        // "any shape allowed here" escape hatch).
        if let Some(additional_schema) = schema.get("additionalProperties").filter(|v| v.is_object()) {
            if let Some(obj) = value.as_object_mut() {
                for (key, child) in obj.iter_mut() {
                    let already_covered = known_properties.is_some_and(|p| p.contains_key(key));
                    if !already_covered {
                        default_in_place(additional_schema, child);
                    }
                }
            }
        }
        return;
    }

    if value.is_array() {
        if let Some(items_schema) = schema.get("items") {
            if let Some(arr) = value.as_array_mut() {
                for item in arr.iter_mut() {
                    default_in_place(items_schema, item);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_missing_scalar_field_is_filled_from_its_default() {
        let schema = json!({"type": "object", "properties": {"replicas": {"type": "integer", "default": 1}}});
        let value = json!({});
        assert_eq!(apply_defaults(&schema, &value), json!({"replicas": 1}));
    }

    #[test]
    fn an_explicitly_submitted_value_is_left_alone() {
        let schema = json!({"type": "object", "properties": {"replicas": {"type": "integer", "default": 1}}});
        let value = json!({"replicas": 5});
        assert_eq!(apply_defaults(&schema, &value), json!({"replicas": 5}));
    }

    #[test]
    fn an_explicit_null_is_replaced_by_the_default_too() {
        let schema = json!({"type": "object", "properties": {"replicas": {"type": "integer", "default": 1}}});
        let value = json!({"replicas": null});
        assert_eq!(apply_defaults(&schema, &value), json!({"replicas": 1}));
    }

    #[test]
    fn defaulting_recurses_into_nested_objects() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {"type": "object", "properties": {"size": {"type": "string", "default": "small"}}},
            },
        });
        let value = json!({"spec": {}});
        assert_eq!(apply_defaults(&schema, &value), json!({"spec": {"size": "small"}}));
    }

    #[test]
    fn a_default_value_is_not_itself_further_defaulted_into() {
        // Real upstream's own rule: applying a default is a leaf
        // operation, not a recursion point -- the operator-authored
        // default is already complete.
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {"type": "object", "default": {}, "properties": {"size": {"type": "string", "default": "small"}}},
            },
        });
        let value = json!({});
        assert_eq!(apply_defaults(&schema, &value), json!({"spec": {}}));
    }

    #[test]
    fn defaulting_recurses_into_array_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "ports": {"type": "array", "items": {"type": "object", "properties": {"protocol": {"type": "string", "default": "TCP"}}}},
            },
        });
        let value = json!({"ports": [{}, {"protocol": "UDP"}]});
        assert_eq!(apply_defaults(&schema, &value), json!({"ports": [{"protocol": "TCP"}, {"protocol": "UDP"}]}));
    }

    #[test]
    fn defaulting_applies_additional_properties_schema_to_map_shaped_fields() {
        let schema = json!({
            "type": "object",
            "properties": {
                "labels": {"type": "object", "additionalProperties": {"type": "object", "properties": {"weight": {"type": "integer", "default": 0}}}},
            },
        });
        let value = json!({"labels": {"a": {}, "b": {"weight": 9}}});
        assert_eq!(apply_defaults(&schema, &value), json!({"labels": {"a": {"weight": 0}, "b": {"weight": 9}}}));
    }

    #[test]
    fn a_bare_boolean_additional_properties_is_not_defaulted_into() {
        let schema = json!({"type": "object", "properties": {"data": {"type": "object", "additionalProperties": true}}});
        let value = json!({"data": {"whatever": "shape"}});
        assert_eq!(apply_defaults(&schema, &value), value);
    }

    #[test]
    fn a_field_with_no_default_and_no_submitted_value_stays_absent() {
        let schema = json!({"type": "object", "properties": {"size": {"type": "string"}}});
        let value = json!({});
        assert_eq!(apply_defaults(&schema, &value), json!({}));
    }
}
