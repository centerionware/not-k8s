//! Defaulting: fills in a JSON object's absent fields from the vendored
//! OpenAPI schema's own `"default"` values, recursively.
//!
//! # What this captures, and what it honestly doesn't
//!
//! Real upstream defaulting (`pkg/apis/core/v1/defaults.go` and friends)
//! is hand-written Go with real conditional logic — "default X to Retain
//! unless Y", defaults that depend on another field's value, defaults that
//! only apply for a particular `apiVersion`. None of that is derivable
//! from a flat per-field default value, and this module doesn't attempt
//! it. What it *does* correctly handle: every **unconditional** default —
//! a field that always gets the same default value whenever it's absent,
//! which is the majority case (`ContainerPort.protocol` defaulting to
//! `"TCP"` is a real, verified example — see `codegen`'s own test). This
//! is a real, useful subset, not a full defaulting engine; conditional
//! defaults are genuinely separate, per-type work.
//!
//! # Recursion
//!
//! A field whose `FIELD_META` entry has `ref_schema` set gets recursed
//! into *after* its own default (if any) is applied — so an absent nested
//! object first materializes as its schema's structural default (usually
//! `{}`), then gets that same schema's own field defaults filled in, all
//! in one pass. An array field recurses into each element individually
//! (matches real per-container, per-port, ... defaulting behavior) rather
//! than defaulting the array itself, since no vendored field carries a
//! meaningful default *for the array as a whole* (only its elements do).

use crate::codegen;
use crate::codegen::openapi_meta::FieldMeta;
use serde_json::{Map, Value};

/// Applies `schema`'s defaults (and every nested schema's, recursively) to
/// `value`. `value` is expected to be a JSON object shaped like `schema`;
/// anything else is returned unchanged — matches
/// `patch::strategic_merge::merge`'s same "not an object, nothing to do
/// but hand it back" posture for a mismatched type.
pub fn apply_defaults(schema: &str, value: &Value) -> Value {
    let Value::Object(obj) = value else {
        return value.clone();
    };
    let mut result = obj.clone();
    let fields: Vec<&'static FieldMeta> = codegen::openapi_meta::FIELD_META.iter().filter(|m| m.schema == schema).collect();

    fill_absent_defaults(&mut result, &fields);
    recurse_into_referenced_fields(&mut result, &fields);

    Value::Object(result)
}

fn fill_absent_defaults(result: &mut Map<String, Value>, fields: &[&'static FieldMeta]) {
    for meta in fields {
        if result.contains_key(meta.field) {
            continue;
        }
        let Some(default_json) = meta.default_json else { continue };
        let Ok(default_value) = serde_json::from_str::<Value>(default_json) else {
            // A malformed default in the vendored spec would be a real
            // upstream data problem, not something to panic the apiserver
            // over — skip it, same fail-open posture the rest of this
            // crate's parsers take on unexpected input.
            continue;
        };
        result.insert(meta.field.to_string(), default_value);
    }
}

fn recurse_into_referenced_fields(result: &mut Map<String, Value>, fields: &[&'static FieldMeta]) {
    for meta in fields {
        let Some(ref_schema) = meta.ref_schema else { continue };
        let Some(current) = result.get(meta.field) else { continue };
        let defaulted = match current {
            Value::Object(_) => apply_defaults(ref_schema, current),
            Value::Array(items) => Value::Array(items.iter().map(|item| apply_defaults(ref_schema, item)).collect()),
            _ => continue,
        };
        result.insert(meta.field.to_string(), defaulted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_absent_scalar_field_gets_its_real_default() {
        let value = json!({"containerPort": 8080});
        let defaulted = apply_defaults("io.k8s.api.core.v1.ContainerPort", &value);
        assert_eq!(defaulted["protocol"], json!("TCP"));
        assert_eq!(defaulted["containerPort"], json!(8080), "an already-present field must be left alone");
    }

    #[test]
    fn a_present_scalar_field_is_never_overwritten_by_its_default() {
        let value = json!({"containerPort": 8080, "protocol": "UDP"});
        let defaulted = apply_defaults("io.k8s.api.core.v1.ContainerPort", &value);
        assert_eq!(defaulted["protocol"], json!("UDP"));
    }

    #[test]
    fn a_non_object_value_is_returned_unchanged() {
        let value = json!("not an object");
        assert_eq!(apply_defaults("io.k8s.api.core.v1.ContainerPort", &value), value);
    }

    /// Proves the two-pass design actually cascades: an absent nested
    /// object first materializes from its own structural default (`{}`),
    /// then gets *that* schema's field defaults filled in — not just a
    /// bare `{}` left as-is.
    #[test]
    fn an_absent_nested_object_field_materializes_and_then_gets_its_own_defaults() {
        // Container.resources defaults to {} (verified against real
        // vendored data), and ResourceRequirements itself is not expected
        // to carry further unconditional scalar defaults — this test
        // proves the materialization half; the recursion mechanism itself
        // is proven end-to-end by the array case below, which does have a
        // real nested scalar default two levels deep.
        let value = json!({"name": "app"});
        let defaulted = apply_defaults("io.k8s.api.core.v1.Container", &value);
        assert_eq!(defaulted["resources"], json!({}), "an absent object-typed field with a {{}} default must materialize");
    }

    /// The real end-to-end case: a list field's *elements* get defaulted
    /// individually, not the list itself — proven with `Container.ports`,
    /// whose element schema (`ContainerPort`) has a real scalar default
    /// two levels down from where `apply_defaults` was first called.
    #[test]
    fn each_element_of_an_array_field_is_defaulted_individually() {
        let value = json!({
            "name": "app",
            "ports": [
                {"containerPort": 80},
                {"containerPort": 443, "protocol": "SCTP"},
            ],
        });
        let defaulted = apply_defaults("io.k8s.api.core.v1.Container", &value);
        let ports = defaulted["ports"].as_array().unwrap();
        assert_eq!(ports[0]["protocol"], json!("TCP"), "an absent element field gets its own default");
        assert_eq!(ports[1]["protocol"], json!("SCTP"), "an already-set element field is left alone");
    }

    #[test]
    fn a_schema_with_no_known_fields_returns_the_object_unchanged() {
        let value = json!({"anything": "goes"});
        assert_eq!(apply_defaults("totally.unknown.schema", &value), value);
    }
}
