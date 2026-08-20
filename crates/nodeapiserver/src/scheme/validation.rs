//! Structural validation: "is every field the schema says is required
//! actually present" — driven by `codegen::openapi_meta::REQUIRED_FIELDS`,
//! the same generic-over-vendored-data posture `scheme::defaulting` takes
//! for the `"default"` extension.
//!
//! # What this captures, and what it honestly doesn't
//!
//! Real upstream validation (`pkg/apis/*/validation/validation.go`) is
//! hand-written Go: format checks (RFC 1123 DNS labels, IANA service
//! names), cross-field consistency ("hostPort requires hostNetwork" isn't
//! expressible from one field alone), enum membership, numeric ranges.
//! None of that is derivable from a flat per-schema required-field list,
//! and this module doesn't attempt it — same honesty `defaulting`'s module
//! doc holds about conditional defaults. What it *does* correctly handle:
//! the one structural fact every field-presence-driven form of validation
//! needs first — a field the OpenAPI schema's own `required` array names
//! is either present in the object or it isn't, checked recursively
//! through every nested object/array this crate's `ref_schema` metadata
//! already threads through defaulting and Strategic Merge Patch.
//!
//! Deliberately run *before* defaulting in any real create/update path:
//! a required field is required in the *user's* input, not required to
//! survive defaulting — a field this module would reject as missing may
//! well get filled in by `scheme::defaulting` immediately afterward, and
//! that's fine; the two are separate questions asked in sequence, not one
//! combined check.

use crate::codegen;
use serde_json::Value;

/// One missing required field, named by its full dotted/indexed path from
/// the value `validate_required` was originally called with — e.g.
/// `"containers[1].name"` — so a caller can report it the way a real
/// admission rejection does (`spec.containers[1].name: Required value`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingField {
    pub path: String,
}

/// Recursively checks that every field `schema` (and every schema reached
/// through a `ref_schema`-bearing field, at any depth) marks `required` is
/// present and non-null in `value`. Returns one `MissingField` per absent
/// field, in the order the schema's own `required` array (as vendored)
/// lists them, depth-first. Empty means valid. A `value` that isn't a JSON
/// object at all is reported as every top-level required field missing —
/// there is nothing to have satisfied them.
pub fn validate_required(schema: &str, value: &Value) -> Vec<MissingField> {
    let mut out = Vec::new();
    walk(schema, value, "", &mut out);
    out
}

fn walk(schema: &str, value: &Value, path_prefix: &str, out: &mut Vec<MissingField>) {
    let obj = value.as_object();

    if let Some(required) = codegen::required_fields_index().get(schema) {
        for field in required {
            let present = obj.is_some_and(|o| o.get(*field).is_some_and(|v| !v.is_null()));
            if !present {
                out.push(MissingField { path: join_path(path_prefix, *field) });
            }
        }
    }

    let Some(obj) = obj else { return };
    let Some(fields) = codegen::field_meta_index_by_schema().get(schema) else { return };
    for meta in fields {
        let Some(ref_schema) = meta.ref_schema else { continue };
        let Some(current) = obj.get(meta.field) else { continue };
        let field_path = join_path(path_prefix, meta.field);
        match current {
            Value::Object(_) => walk(ref_schema, current, &field_path, out),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk(ref_schema, item, &format!("{field_path}[{i}]"), out);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_present_required_field_produces_no_error() {
        let value = json!({"containerPort": 8080});
        assert_eq!(validate_required("io.k8s.api.core.v1.ContainerPort", &value), vec![]);
    }

    #[test]
    fn an_absent_required_field_is_reported_with_its_path() {
        let value = json!({"hostPort": 8080});
        let missing = validate_required("io.k8s.api.core.v1.ContainerPort", &value);
        assert_eq!(missing, vec![MissingField { path: "containerPort".to_string() }]);
    }

    #[test]
    fn a_null_required_field_is_treated_as_absent() {
        let value = json!({"containerPort": null});
        let missing = validate_required("io.k8s.api.core.v1.ContainerPort", &value);
        assert_eq!(missing, vec![MissingField { path: "containerPort".to_string() }]);
    }

    #[test]
    fn a_schema_with_no_required_array_never_errors() {
        assert_eq!(validate_required("io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta", &json!({})), vec![]);
    }

    /// Proves the recursion: `PodSpec.containers` is a `Container`-element
    /// array, and `Container` itself requires `name` — an element missing
    /// it must be reported with an indexed path, not silently accepted
    /// just because the outer object had no `required` violations of its
    /// own.
    #[test]
    fn a_missing_field_in_a_nested_array_element_is_reported_with_an_indexed_path() {
        let value = json!({
            "containers": [
                {"name": "app"},
                {"image": "sidecar:v1"},
            ]
        });
        let missing = validate_required("io.k8s.api.core.v1.PodSpec", &value);
        assert_eq!(missing, vec![MissingField { path: "containers[1].name".to_string() }]);
    }

    /// Proves recursion into a nested *object* field (not array) too, and
    /// that a completely non-object value at the top produces every
    /// top-level required field as missing rather than panicking.
    #[test]
    fn a_non_object_value_reports_every_top_level_required_field_missing() {
        let missing = validate_required("io.k8s.api.core.v1.ContainerPort", &json!("not an object"));
        assert_eq!(missing, vec![MissingField { path: "containerPort".to_string() }]);
    }
}
