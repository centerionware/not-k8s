//! Structural-schema required/type validation for CRD-defined objects —
//! the dynamic, runtime-schema counterpart to `scheme::validation`
//! (which walks this crate's own *compiled* `REQUIRED_FIELDS`/
//! `TYPE_INFO` tables for built-in types), the same "land the runtime-
//! schema-walking sibling next to the compiled one" split
//! `apiextensions::schema_defaults` already established for defaulting.
//! Produces the exact same [`crate::scheme::validation::MissingField`]/
//! [`crate::scheme::validation::TypeMismatch`] violation types the
//! compiled-schema checks do, so every real call site
//! (`server::rest::create`/`update`/`patch_persist`'s own violation
//! formatting) already knows how to report either kind identically —
//! this module adds no new violation shape of its own.
//!
//! Same two checks, same split between them, same reasoning
//! `scheme::validation`'s own module doc comment gives for why real
//! upstream's hand-written validation (format checks, cross-field
//! consistency, enum membership, numeric ranges) isn't attempted here
//! either — `x-kubernetes-validations` CEL is the real mechanism a CRD
//! schema has for anything past "is this field present" and "is this
//! field the right JSON kind," and that's Group K's own still-not-landed
//! CEL work (needs the cost budget built first), not this module's job.

use crate::scheme::validation::{MissingField, TypeMismatch};
use serde_json::Value;

/// Recursively checks that every field `schema`'s own `required` array
/// (at any depth, following `properties`) names is present and non-null
/// in `value`. Empty means valid — same convention
/// `scheme::validation::validate_required` uses.
pub fn validate_required(schema: &Value, value: &Value) -> Vec<MissingField> {
    let mut out = Vec::new();
    walk_required(schema, value, "", &mut out);
    out
}

fn walk_required(schema: &Value, value: &Value, path_prefix: &str, out: &mut Vec<MissingField>) {
    let obj = value.as_object();

    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for field in required.iter().filter_map(Value::as_str) {
            let present = obj.is_some_and(|o| o.get(field).is_some_and(|v| !v.is_null()));
            if !present {
                out.push(MissingField { path: join_path(path_prefix, field) });
            }
        }
    }

    let Some(obj) = obj else { return };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else { return };
    for (field, field_schema) in properties {
        let Some(current) = obj.get(field) else { continue };
        let field_path = join_path(path_prefix, field);
        match current {
            Value::Object(_) => walk_required(field_schema, current, &field_path, out),
            Value::Array(items) => {
                if let Some(items_schema) = field_schema.get("items") {
                    for (i, item) in items.iter().enumerate() {
                        walk_required(items_schema, item, &format!("{field_path}[{i}]"), out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn join_path(prefix: &str, field: &str) -> String {
    if prefix.is_empty() {
        field.to_string()
    } else {
        format!("{prefix}.{field}")
    }
}

/// Recursively checks that every present field whose schema declares a
/// `type` actually has that JSON kind — an absent field is
/// [`validate_required`]'s concern, not this function's, same split
/// `scheme::validation::validate_types` already establishes.
pub fn validate_types(schema: &Value, value: &Value) -> Vec<TypeMismatch> {
    let mut out = Vec::new();
    walk_types(schema, value, "", &mut out);
    out
}

fn walk_types(schema: &Value, value: &Value, path_prefix: &str, out: &mut Vec<TypeMismatch>) {
    let Some(obj) = value.as_object() else { return };
    let properties = schema.get("properties").and_then(Value::as_object);

    for (field, field_value) in obj {
        if field_value.is_null() {
            continue;
        }
        let field_path = join_path(path_prefix, field);
        let Some(field_schema) = properties.and_then(|p| p.get(field)) else { continue };

        if let Some(expected) = field_schema.get("type").and_then(Value::as_str) {
            if !matches_kind(expected, field_value) {
                out.push(TypeMismatch { path: field_path.clone(), expected: expected.to_string(), actual_kind: kind_name(field_value).to_string() });
                // Same posture `scheme::validation::walk_types` takes: a
                // value whose top-level kind is already wrong has nothing
                // sane to recurse into.
                continue;
            }
        }

        match field_value {
            Value::Object(_) => walk_types(field_schema, field_value, &field_path, out),
            Value::Array(items) => {
                if let Some(items_schema) = field_schema.get("items") {
                    for (i, item) in items.iter().enumerate() {
                        walk_types(items_schema, item, &format!("{field_path}[{i}]"), out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Identical rule set to `scheme::validation`'s own `matches_kind` —
/// kept as its own copy (not a shared function) since that one is
/// private to a different module and the two schemas' own `type`
/// strings come from genuinely different sources (a compiled
/// `TYPE_INFO` table vs. a runtime `openAPIV3Schema`) even though the
/// OpenAPI type vocabulary itself is identical.
fn matches_kind(openapi_type: &str, value: &Value) -> bool {
    match openapi_type {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        // JSON has no separate integer literal syntax -- structurally any
        // numeric value with a zero fractional part counts, same
        // reasoning `scheme::validation`'s own doc comment gives.
        "integer" => value.as_f64().is_some_and(|f| f.fract() == 0.0),
        "array" => value.is_array(),
        "object" => value.is_object(),
        // An unrecognized/absent type constrains nothing -- fail-open,
        // same posture every other generically-derived check in this
        // crate takes on data it doesn't have a rule for.
        _ => true,
    }
}

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn widget_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "required": ["color"],
                    "properties": {
                        "color": {"type": "string"},
                        "replicas": {"type": "integer"},
                        "ports": {"type": "array", "items": {"type": "object", "required": ["number"], "properties": {"number": {"type": "integer"}}}},
                    },
                },
            },
        })
    }

    #[test]
    fn a_present_required_field_produces_no_violation() {
        let value = json!({"spec": {"color": "red"}});
        assert_eq!(validate_required(&widget_schema(), &value), vec![]);
    }

    #[test]
    fn an_absent_required_field_is_reported_with_its_full_path() {
        let value = json!({"spec": {}});
        let violations = validate_required(&widget_schema(), &value);
        assert_eq!(violations, vec![MissingField { path: "spec.color".to_string() }]);
    }

    #[test]
    fn required_validation_recurses_into_array_items() {
        let value = json!({"spec": {"color": "red", "ports": [{}]}});
        let violations = validate_required(&widget_schema(), &value);
        assert_eq!(violations, vec![MissingField { path: "spec.ports[0].number".to_string() }]);
    }

    #[test]
    fn a_field_with_the_right_json_kind_produces_no_type_violation() {
        let value = json!({"spec": {"color": "red", "replicas": 3}});
        assert_eq!(validate_types(&widget_schema(), &value), vec![]);
    }

    #[test]
    fn a_field_with_the_wrong_json_kind_is_reported() {
        let value = json!({"spec": {"color": 5}});
        let violations = validate_types(&widget_schema(), &value);
        assert_eq!(violations, vec![TypeMismatch { path: "spec.color".to_string(), expected: "string".to_string(), actual_kind: "number".to_string() }]);
    }

    #[test]
    fn an_integer_typed_field_accepts_a_whole_number() {
        let value = json!({"spec": {"color": "red", "replicas": 3.0}});
        assert_eq!(validate_types(&widget_schema(), &value), vec![]);
    }

    #[test]
    fn an_integer_typed_field_rejects_a_fractional_number() {
        let value = json!({"spec": {"color": "red", "replicas": 3.5}});
        let violations = validate_types(&widget_schema(), &value);
        assert_eq!(violations, vec![TypeMismatch { path: "spec.replicas".to_string(), expected: "integer".to_string(), actual_kind: "number".to_string() }]);
    }

    #[test]
    fn a_field_with_no_schema_entry_at_all_is_not_flagged() {
        let value = json!({"spec": {"color": "red", "unknownField": 42}});
        assert_eq!(validate_types(&widget_schema(), &value), vec![]);
    }

    #[test]
    fn an_explicit_null_is_not_flagged_as_a_type_mismatch() {
        let value = json!({"spec": {"color": null}});
        assert_eq!(validate_types(&widget_schema(), &value), vec![]);
    }
}
