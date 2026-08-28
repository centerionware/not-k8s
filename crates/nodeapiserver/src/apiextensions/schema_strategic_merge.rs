//! Strategic Merge Patch for CRD-defined objects — the runtime-schema
//! sibling of `crate::patch::strategic_merge` (which walks this crate's
//! own *compiled* `FIELD_META`/`ref_schema` table for built-in types),
//! same split `apiextensions::schema_defaults`/`schema_validation`/
//! `schema_pruning` already established for their own compiled
//! counterparts.
//!
//! Same semantics as the compiled implementation (see that module's own
//! doc comment for the full list): `null` deletes a key, an object field
//! merges recursively, every other field replaces wholesale — except a
//! **list** field merges by key when its schema's own
//! `x-kubernetes-list-type: map` names one or more
//! `x-kubernetes-list-map-keys`, real upstream's own structural-schema
//! replacement for the compiled `patch_strategy`/`patch_merge_key`
//! annotations built-in types carry instead. **A genuine improvement
//! over the compiled path, not an inconsistency**: `x-kubernetes-list-
//! map-keys` is a real array (real upstream supports composite keys —
//! e.g. `containerName` + `path` together identifying one
//! `volumeMount`), so this module matches *every* listed key rather than
//! the compiled path's single `patch_merge_key` (built-in types in the
//! vendored spec never actually need more than one key, which is why
//! that simplification was safe there, not a reason to repeat it here
//! where the real annotation supports more).

use serde_json::Value;

/// Applies a strategic merge patch to `original`, shaped like `schema` —
/// see this module's own doc comment for the semantics.
pub fn apply(schema: &Value, original: &Value, patch: &Value) -> Value {
    merge(schema, original, patch)
}

fn merge(schema: &Value, original: &Value, patch: &Value) -> Value {
    let (Value::Object(orig_map), Value::Object(patch_map)) = (original, patch) else {
        // Types don't align to merge -- the patch's value wins wholesale,
        // same JSON-Merge-Patch-style fallback the compiled
        // implementation uses.
        return patch.clone();
    };

    match patch_map.get("$patch").and_then(Value::as_str) {
        Some("delete") => return Value::Null,
        Some("replace") => return Value::Object(without_directives(patch_map)),
        _ => {}
    }

    let properties = schema.get("properties").and_then(Value::as_object);
    let mut result = orig_map.clone();
    apply_primitive_list_deletes(&mut result, patch_map);
    for (key, patch_value) in patch_map {
        if key.starts_with('$') {
            continue;
        }
        if patch_value.is_null() {
            result.remove(key);
            continue;
        }

        let field_schema = properties.and_then(|p| p.get(key));
        let existing = result.get(key).cloned();

        let merged = match (existing, patch_value) {
            (Some(Value::Array(orig_list)), Value::Array(patch_list)) if is_merge_list(field_schema) => {
                // `is_merge_list` already confirmed `field_schema` is
                // `Some` and names a real `x-kubernetes-list-map-keys`.
                Value::Array(merge_list(field_schema.expect("checked by is_merge_list"), &orig_list, patch_list))
            }
            (Some(orig_value @ Value::Object(_)), Value::Object(_)) => {
                // Recurse using *this field's own* schema when the
                // parent names it in `properties`; an unrecognized field
                // (e.g. the parent used a schema-shaped
                // `additionalProperties` instead) merges generically —
                // same fallback reasoning the compiled implementation's
                // own doc comment gives for an unknown `ref_schema`.
                let empty = Value::Null;
                merge(field_schema.unwrap_or(&empty), &orig_value, patch_value)
            }
            _ => patch_value.clone(),
        };
        if merged.is_null() {
            result.remove(key);
        } else {
            result.insert(key.clone(), merged);
        }
    }
    for (key, order) in patch_map.iter().filter_map(|(key, value)| key.strip_prefix("$setElementOrder/").map(|field| (field, value))) {
        let Some(existing) = result.get(key).and_then(Value::as_array).map(|existing| existing.to_vec()) else { continue };
        let Some(field_schema) = properties.and_then(|p| p.get(key)) else { continue };
        let Some(merge_key) = merge_keys(field_schema).first().copied() else { continue };
        result.insert(key.to_string(), Value::Array(reorder_list(&existing, order, merge_key)));
    }
    Value::Object(result)
}

fn without_directives(map: &serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    map.iter().filter(|(key, _)| !key.starts_with('$')).map(|(key, value)| (key.clone(), value.clone())).collect()
}

fn apply_primitive_list_deletes(result: &mut serde_json::Map<String, Value>, patch: &serde_json::Map<String, Value>) {
    for (field, values) in patch.iter().filter_map(|(key, value)| key.strip_prefix("$deleteFromPrimitiveList/").map(|field| (field, value))) {
        let Some(values) = values.as_array() else { continue };
        let Some(existing) = result.get_mut(field).and_then(Value::as_array_mut) else { continue };
        existing.retain(|value| !values.iter().any(|deleted| deleted == value));
    }
}

fn reorder_list(existing: &[Value], order: &Value, merge_key: &str) -> Vec<Value> {
    let Some(order) = order.as_array() else { return existing.to_vec() };
    let mut remaining = existing.to_vec();
    let mut ordered = Vec::with_capacity(existing.len());
    for requested in order {
        let Some(value) = requested.get(merge_key) else { continue };
        if let Some(index) = remaining.iter().position(|item| item.get(merge_key) == Some(value)) {
            ordered.push(remaining.remove(index));
        }
    }
    ordered.extend(remaining);
    ordered
}

/// True when `field_schema` names `x-kubernetes-list-type: "map"` with a
/// non-empty `x-kubernetes-list-map-keys` — real upstream's own
/// structural-schema signal that a list field merges by key rather than
/// being replaced wholesale (`"set"`/`"atomic"`, or no
/// `x-kubernetes-list-type` at all, both mean "not a merge list" here,
/// matching real upstream's own default of atomic replacement).
fn is_merge_list(field_schema: Option<&Value>) -> bool {
    let Some(schema) = field_schema else { return false };
    schema.get("x-kubernetes-list-type").and_then(Value::as_str) == Some("map") && !merge_keys(schema).is_empty()
}

fn merge_keys(field_schema: &Value) -> Vec<&str> {
    field_schema.get("x-kubernetes-list-map-keys").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect()
}

/// Merges a `x-kubernetes-list-type: map` list: a patch element whose
/// *every* `x-kubernetes-list-map-keys` value matches an existing
/// element's own merges into it (recursively, using the list's own
/// `items` schema); one that doesn't match (or is missing one of the
/// key fields entirely, or isn't a JSON object at all) appends. `merge_keys`
/// being non-empty and every element being an object is already
/// guaranteed by [`is_merge_list`]'s own check before this is ever
/// called — the per-element object/key-presence checks below are this
/// function's own defensive handling of one non-conforming element, not
/// a whole-list fallback the way the compiled implementation's
/// `merge_list` degrades to wholesale replacement.
fn merge_list(field_schema: &Value, original: &[Value], patch: &[Value]) -> Vec<Value> {
    let keys = merge_keys(field_schema);
    let empty = Value::Null;
    let items_schema = field_schema.get("items").unwrap_or(&empty);

    let replace = patch.iter().any(|item| item.get("$patch").and_then(Value::as_str) == Some("replace"));
    let mut result = if replace { Vec::new() } else { original.to_vec() };
    'patch_elements: for patch_elem in patch {
        let directive = patch_elem.get("$patch").and_then(Value::as_str);
        if directive == Some("replace") {
            continue;
        }
        let Some(patch_key_values) = key_values(patch_elem, &keys) else {
            if directive == Some("delete") {
                continue;
            }
            result.push(patch_elem.clone());
            continue;
        };
        if directive == Some("delete") {
            result.retain(|existing| key_values(existing, &keys).as_deref() != Some(&patch_key_values));
            continue;
        }
        let cleaned_patch_elem = patch_elem.clone();
        for existing in result.iter_mut() {
            if key_values(existing, &keys).as_deref() == Some(&patch_key_values) {
                *existing = merge(items_schema, existing, &cleaned_patch_elem);
                continue 'patch_elements;
            }
        }
        result.push(cleaned_patch_elem);
    }
    result
}

/// `None` if `element` isn't an object, or is missing any one of `keys`
/// — either way, it has no identity `merge_list` can match by.
fn key_values<'a>(element: &'a Value, keys: &[&str]) -> Option<Vec<&'a Value>> {
    let obj = element.as_object()?;
    keys.iter().map(|k| obj.get(*k)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn widget_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "color": {"type": "string"},
                "size": {"type": "string"},
                "nested": {"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"}}},
                "ports": {
                    "type": "array",
                    "x-kubernetes-list-type": "map",
                    "x-kubernetes-list-map-keys": ["name"],
                    "items": {"type": "object", "properties": {"name": {"type": "string"}, "port": {"type": "integer"}}},
                },
                "unannotatedList": {"type": "array", "items": {"type": "object"}},
            },
        })
    }

    #[test]
    fn a_scalar_field_is_replaced_wholesale() {
        let merged = apply(&widget_schema(), &json!({"color": "red"}), &json!({"color": "blue"}));
        assert_eq!(merged, json!({"color": "blue"}));
    }

    #[test]
    fn a_null_value_deletes_the_field() {
        let merged = apply(&widget_schema(), &json!({"color": "red", "size": "small"}), &json!({"color": null}));
        assert_eq!(merged, json!({"size": "small"}));
    }

    #[test]
    fn a_nested_object_field_merges_recursively() {
        let merged = apply(&widget_schema(), &json!({"nested": {"a": "1", "b": "2"}}), &json!({"nested": {"a": "9"}}));
        assert_eq!(merged, json!({"nested": {"a": "9", "b": "2"}}));
    }

    #[test]
    fn a_list_with_no_list_type_annotation_is_replaced_wholesale() {
        let merged = apply(&widget_schema(), &json!({"unannotatedList": [{"x": 1}, {"x": 2}]}), &json!({"unannotatedList": [{"x": 3}]}));
        assert_eq!(merged, json!({"unannotatedList": [{"x": 3}]}));
    }

    #[test]
    fn a_map_type_list_merges_a_matching_element_by_its_key_and_appends_a_new_one() {
        let original = json!({"ports": [{"name": "http", "port": 80}, {"name": "https", "port": 443}]});
        let patch = json!({"ports": [{"name": "http", "port": 8080}, {"name": "metrics", "port": 9090}]});
        let merged = apply(&widget_schema(), &original, &patch);
        assert_eq!(
            merged,
            json!({"ports": [
                {"name": "http", "port": 8080},
                {"name": "https", "port": 443},
                {"name": "metrics", "port": 9090},
            ]})
        );
    }

    #[test]
    fn a_map_type_list_element_missing_its_own_key_field_is_appended_not_matched() {
        let original = json!({"ports": [{"name": "http", "port": 80}]});
        let patch = json!({"ports": [{"port": 22}]});
        let merged = apply(&widget_schema(), &original, &patch);
        let ports = merged["ports"].as_array().unwrap();
        assert_eq!(ports.len(), 2, "an unidentifiable element must append, not overwrite: {ports:?}");
    }

    #[test]
    fn a_composite_key_list_matches_only_when_every_key_value_agrees() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mounts": {
                    "type": "array",
                    "x-kubernetes-list-type": "map",
                    "x-kubernetes-list-map-keys": ["container", "path"],
                    "items": {"type": "object", "properties": {"container": {"type": "string"}, "path": {"type": "string"}, "readOnly": {"type": "boolean"}}},
                },
            },
        });
        let original = json!({"mounts": [{"container": "app", "path": "/data", "readOnly": false}]});
        // Same container, different path -- must NOT match, must append.
        let patch = json!({"mounts": [{"container": "app", "path": "/other", "readOnly": true}]});
        let merged = apply(&schema, &original, &patch);
        assert_eq!(merged["mounts"].as_array().unwrap().len(), 2);

        // Exact same composite key -- must match and merge.
        let patch2 = json!({"mounts": [{"container": "app", "path": "/data", "readOnly": true}]});
        let merged2 = apply(&schema, &original, &patch2);
        assert_eq!(merged2["mounts"].as_array().unwrap().len(), 1);
        assert_eq!(merged2["mounts"][0]["readOnly"], true);
    }

    #[test]
    fn patch_replace_discards_unmentioned_runtime_schema_fields() {
        let merged = apply(&widget_schema(), &json!({"color": "red", "size": "large"}), &json!({"$patch": "replace", "color": "blue"}));
        assert_eq!(merged, json!({"color": "blue"}));
    }

    #[test]
    fn runtime_merge_list_supports_delete_and_replace_item_directives() {
        let original = json!({"ports": [{"name": "http", "port": 80}, {"name": "metrics", "port": 9090}]});
        let deleted = apply(&widget_schema(), &original, &json!({"ports": [{"name": "metrics", "$patch": "delete"}]}));
        assert_eq!(deleted["ports"], json!([{"name": "http", "port": 80}]));
        let replaced = apply(&widget_schema(), &original, &json!({"ports": [{"$patch": "replace"}, {"name": "fresh", "port": 8080}]}));
        assert_eq!(replaced["ports"], json!([{"name": "fresh", "port": 8080}]));
    }

    #[test]
    fn runtime_primitive_list_delete_and_element_order_directives_are_applied() {
        let schema = json!({
            "type": "object",
            "properties": {
                "args": {"type": "array", "items": {"type": "string"}},
                "ports": {"type": "array", "x-kubernetes-list-type": "map", "x-kubernetes-list-map-keys": ["name"], "items": {"type": "object"}}
            }
        });
        let original = json!({"args": ["--one", "--two"], "ports": [{"name": "http"}, {"name": "metrics"}]});
        let patch = json!({"$deleteFromPrimitiveList/args": ["--two"], "$setElementOrder/ports": [{"name": "metrics"}, {"name": "http"}]});
        let merged = apply(&schema, &original, &patch);
        assert_eq!(merged["args"], json!(["--one"]));
        assert_eq!(merged["ports"], json!([{"name": "metrics"}, {"name": "http"}]));
    }
}
