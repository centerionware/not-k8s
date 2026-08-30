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
//! The runtime walk also enforces the schema-local constraints represented
//! directly in OpenAPI (`enum`, numeric/string/array/object bounds,
//! `multipleOf`, `pattern`, and the standard scalar formats). Cross-field
//! rules remain the CRD's `x-kubernetes-validations` CEL responsibility.

use crate::scheme::validation::{MissingField, TypeMismatch};
use base64::Engine as _;
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

/// Validates constraints that are local to one OpenAPI schema node.
///
/// Kubernetes rejects these constraints for a CRD object before invoking
/// its CEL rules. The returned strings intentionally follow the same path
/// convention as [`MissingField`] and [`TypeMismatch`] so REST callers can
/// report them alongside the existing structural violations.
pub fn validate_constraints(schema: &Value, value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk_constraints(schema, value, "", &mut out);
    out
}

fn walk_constraints(schema: &Value, value: &Value, path: &str, out: &mut Vec<String>) {
    if value.is_null() {
        return;
    }
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            walk_constraints(branch, value, path, out);
        }
    }
    validate_node_constraints(schema, value, path, out);

    match value {
        Value::Object(object) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let additional = schema.get("additionalProperties");
            for (field, child) in object {
                let child_schema = properties
                    .and_then(|fields| fields.get(field))
                    .or(additional);
                let Some(child_schema) = child_schema else { continue };
                walk_constraints(child_schema, child, &join_path(path, field), out);
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    walk_constraints(item_schema, item, &format!("{path}[{index}]"), out);
                }
            }
        }
        _ => {}
    }
}

fn validate_node_constraints(schema: &Value, value: &Value, path: &str, out: &mut Vec<String>) {
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.iter().any(|candidate| candidate == value) {
            add_violation(path, "must be one of the values declared by the schema", out);
        }
    }

    if let Some(number) = value.as_f64() {
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
            let exclusive = schema.get("exclusiveMinimum").and_then(Value::as_bool).unwrap_or(false);
            if (exclusive && number <= minimum) || (!exclusive && number < minimum) {
                add_violation(path, &format!("must be {} {minimum}", if exclusive { "greater than" } else { "at least" }), out);
            }
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
            let exclusive = schema.get("exclusiveMaximum").and_then(Value::as_bool).unwrap_or(false);
            if (exclusive && number >= maximum) || (!exclusive && number > maximum) {
                add_violation(path, &format!("must be {} {maximum}", if exclusive { "less than" } else { "at most" }), out);
            }
        }
        if let Some(exclusive_minimum) = schema.get("exclusiveMinimum").and_then(Value::as_f64) {
            if number <= exclusive_minimum {
                add_violation(path, &format!("must be greater than {exclusive_minimum}"), out);
            }
        }
        if let Some(exclusive_maximum) = schema.get("exclusiveMaximum").and_then(Value::as_f64) {
            if number >= exclusive_maximum {
                add_violation(path, &format!("must be less than {exclusive_maximum}"), out);
            }
        }
        if let Some(multiple_of) = schema.get("multipleOf").and_then(Value::as_f64) {
            if multiple_of > 0.0 && (number / multiple_of - (number / multiple_of).round()).abs() > 1e-9 {
                add_violation(path, &format!("must be a multiple of {multiple_of}"), out);
            }
        }
    }

    if let Some(string) = value.as_str() {
        let length = string.chars().count();
        check_bound(schema, "minLength", length, |actual, bound| actual < bound, "must be at least", path, out);
        check_bound(schema, "maxLength", length, |actual, bound| actual > bound, "must be at most", path, out);
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            match regex::Regex::new(pattern) {
                Ok(pattern) if !pattern.is_match(string) => add_violation(path, "does not match the schema pattern", out),
                Err(_) => add_violation(path, "uses an invalid schema pattern", out),
                Ok(_) => {}
            }
        }
        if let Some(format) = schema.get("format").and_then(Value::as_str) {
            if !valid_format(format, string) {
                add_violation(path, &format!("must conform to format {format}"), out);
            }
        }
    }

    if let Some(array) = value.as_array() {
        check_bound(schema, "minItems", array.len(), |actual, bound| actual < bound, "must contain at least", path, out);
        check_bound(schema, "maxItems", array.len(), |actual, bound| actual > bound, "must contain at most", path, out);
        if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
            for (index, item) in array.iter().enumerate() {
                if array[..index].iter().any(|previous| previous == item) {
                    add_violation(path, "must contain unique items", out);
                    break;
                }
            }
        }
    }

    if let Some(object) = value.as_object() {
        check_bound(schema, "minProperties", object.len(), |actual, bound| actual < bound, "must contain at least", path, out);
        check_bound(schema, "maxProperties", object.len(), |actual, bound| actual > bound, "must contain at most", path, out);
    }
}

fn check_bound(
    schema: &Value,
    key: &str,
    actual: usize,
    predicate: impl Fn(usize, usize) -> bool,
    description: &str,
    path: &str,
    out: &mut Vec<String>,
) {
    let Some(bound) = schema.get(key).and_then(Value::as_u64).and_then(|bound| usize::try_from(bound).ok()) else { return };
    if predicate(actual, bound) {
        add_violation(path, &format!("{description} {bound}"), out);
    }
}

fn add_violation(path: &str, message: &str, out: &mut Vec<String>) {
    if path.is_empty() {
        out.push(message.to_string());
    } else {
        out.push(format!("{path}: {message}"));
    }
}

fn valid_format(format: &str, value: &str) -> bool {
    match format {
        "byte" => base64::engine::general_purpose::STANDARD.decode(value).is_ok(),
        "date" => chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok(),
        "date-time" => chrono::DateTime::parse_from_rfc3339(value).is_ok(),
        "email" => value.contains('@') && !value.starts_with('@') && !value.ends_with('@'),
        "hostname" | "idn-hostname" => crate::scheme::name_format::is_dns1123_subdomain(value).is_empty(),
        "ipv4" => value.parse::<std::net::Ipv4Addr>().is_ok(),
        "ipv6" => value.parse::<std::net::Ipv6Addr>().is_ok(),
        "uuid" => uuid::Uuid::parse_str(value).is_ok(),
        // Kubernetes treats unknown formats and formats intended only for
        // presentation as advisory rather than rejecting arbitrary strings.
        "password" | "uri" | "uri-reference" | "duration" | "int-or-string" => true,
        _ => true,
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

    #[test]
    fn local_scalar_constraints_are_checked_recursively() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {"type": "object", "properties": {
                    "color": {"type": "string", "enum": ["red", "blue"], "minLength": 3, "pattern": "^[a-z]+$"},
                    "weight": {"type": "number", "minimum": 1, "maximum": 5, "multipleOf": 0.5}
                }}
            }
        });
        let violations = validate_constraints(&schema, &json!({"spec": {"color": "GREEN", "weight": 5.25}}));
        assert!(violations.iter().any(|violation| violation.contains("spec.color") && violation.contains("one of")));
        assert!(violations.iter().any(|violation| violation.contains("spec.color") && violation.contains("pattern")));
        assert!(violations.iter().any(|violation| violation.contains("spec.weight") && violation.contains("multiple")));
    }

    #[test]
    fn collection_constraints_and_formats_are_checked() {
        let schema = json!({
            "type": "object",
            "properties": {
                "addresses": {"type": "array", "minItems": 2, "maxItems": 3, "uniqueItems": true, "items": {"type": "string", "format": "ipv4"}},
                "id": {"type": "string", "format": "uuid"}
            }
        });
        let violations = validate_constraints(&schema, &json!({"addresses": ["127.0.0.1", "127.0.0.1"], "id": "not-a-uuid"}));
        assert!(violations.iter().any(|violation| violation.contains("addresses") && violation.contains("unique")));
        assert!(violations.iter().any(|violation| violation.contains("id") && violation.contains("uuid")));
    }

    #[test]
    fn constraints_follow_all_of_schema_branches() {
        let schema = json!({
            "allOf": [{
                "type": "object",
                "properties": {
                    "token": {"type": "string", "format": "uuid"}
                }
            }]
        });
        let violations = validate_constraints(&schema, &json!({"token": "not-a-uuid"}));
        assert!(violations.iter().any(|violation| violation.contains("token") && violation.contains("uuid")));
    }
}
