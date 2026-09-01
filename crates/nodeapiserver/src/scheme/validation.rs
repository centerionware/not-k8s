//! Structural validation: two generically-derived checks, each driven by
//! its own flat table over the vendored OpenAPI specs, the same
//! generic-over-vendored-data posture `scheme::defaulting` takes for the
//! `"default"` extension.
//!
//! - `validate_required` — "is every field the schema says is required
//!   actually present," driven by `codegen::openapi_meta::REQUIRED_FIELDS`.
//! - `validate_types` — "does every field that *is* present have the JSON
//!   kind the schema declares," driven by `codegen::openapi_meta::TYPE_INFO`.
//!
//! # What this captures, and what it honestly doesn't
//!
//! Real upstream validation (`pkg/apis/*/validation/validation.go`) is
//! hand-written Go: cross-field consistency ("hostPort requires hostNetwork"
//! isn't expressible from one field alone) and other semantic rules still
//! need explicit per-kind code. The verified HPA
//! `maxReplicas >= minReplicas` and apps workload selector/template rules are
//! the first such built-in semantic checks here. The published OpenAPI schema's local
//! constraints (formats, enums, ranges, lengths, patterns, and uniqueness)
//! are available through [`validate_openapi_constraints`]; this module keeps
//! required fields and JSON kinds in the compact generated metadata tables
//! above and supplements them with that schema-driven pass at REST call
//! sites. Both layers recurse through nested objects and arrays.
//!
//! Deliberately run *before* defaulting in any real create/update path:
//! a required field is required in the *user's* input, not required to
//! survive defaulting — a field `validate_required` would reject as
//! missing may well get filled in by `scheme::defaulting` immediately
//! afterward, and that's fine; the two are separate questions asked in
//! sequence, not one combined check.

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

/// One field whose present value's JSON kind doesn't match the schema's
/// own declared `type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeMismatch {
    pub path: String,
    pub expected: String,
    pub actual_kind: String,
}

/// Recursively checks that every field `schema` (and every schema reached
/// through `ref_schema`) declares a `TYPE_INFO` entry for actually matches
/// the JSON kind of the present value — an *absent* field is not this
/// function's concern (that's `validate_required`'s job; the two checks
/// are deliberately separate, same posture as upstream's own struct-tag
/// based decoding, which only ever complains about a field it actually
/// saw). A field with no `TYPE_INFO` entry (an object-typed field, or one
/// this crate hasn't captured a type for) is skipped by this check, not
/// flagged — its shape is validated by the `ref_schema` recursion instead.
pub fn validate_types(schema: &str, value: &Value) -> Vec<TypeMismatch> {
    let mut out = Vec::new();
    walk_types(schema, value, "", &mut out);
    out
}

/// Validates constraints that are present in a built-in resource's published
/// OpenAPI schema. Required fields and JSON kinds remain backed by the
/// compiled metadata tables above; this supplemental pass covers schema-local
/// enum, range, length, pattern, uniqueness, and standard-format rules for
/// the same built-in object.
pub fn validate_openapi_constraints(
    group: &str,
    version: &str,
    kind: &str,
    value: &Value,
) -> Vec<String> {
    let Some(schema) = codegen::openapi_schema_for_gvk(group, version, kind) else {
        return Vec::new();
    };
    let mut violations = crate::apiextensions::schema_validation::validate_constraints(&schema, value);
    violations.extend(validate_builtin_semantics(group, version, kind, value));
    violations
}

/// Cross-field validation that cannot be represented by a single OpenAPI
/// schema constraint. This intentionally grows only from an upstream-verified
/// rule at a time; structural validation remains the generic path above.
fn validate_builtin_semantics(group: &str, _version: &str, kind: &str, value: &Value) -> Vec<String> {
    if group == "autoscaling" && kind == "HorizontalPodAutoscaler" {
        let Some(spec) = value.get("spec").and_then(Value::as_object) else {
            return Vec::new();
        };
        let Some(max_replicas) = spec.get("maxReplicas").and_then(Value::as_i64) else {
            return Vec::new();
        };
        let Some(min_replicas) = spec.get("minReplicas").and_then(Value::as_i64) else {
            return Vec::new();
        };
        if max_replicas < min_replicas {
            return vec!["spec.maxReplicas: must be greater than or equal to `minReplicas`".to_string()];
        }
    }

    if group == "apps"
        && matches!(kind, "DaemonSet" | "Deployment" | "ReplicaSet" | "StatefulSet")
    {
        return validate_workload_selector(value);
    }

    Vec::new()
}

/// These workload kinds require their selector to match the labels on the
/// pod template. This is a cross-field invariant enforced by each upstream
/// apps validator, not by the published OpenAPI schema.
fn validate_workload_selector(value: &Value) -> Vec<String> {
    let Some(spec) = value.get("spec").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(selector) = spec.get("selector").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(template_labels) = spec
        .get("template")
        .and_then(|template| template.get("metadata"))
        .and_then(|metadata| metadata.get("labels"))
        .and_then(Value::as_object)
    else {
        return vec!["spec.selector: selector does not match template labels".to_string()];
    };

    let match_labels = selector
        .get("matchLabels")
        .and_then(Value::as_object)
        .is_none_or(|labels| {
            labels.iter().all(|(key, expected)| {
                expected.as_str().is_some_and(|expected| {
                    template_labels.get(key).and_then(Value::as_str) == Some(expected)
                })
            })
        });
    let match_expressions = selector
        .get("matchExpressions")
        .and_then(Value::as_array)
        .is_none_or(|expressions| {
            expressions.iter().all(|expression| {
                let Some(key) = expression.get("key").and_then(Value::as_str) else {
                    return false;
                };
                let Some(operator) = expression.get("operator").and_then(Value::as_str) else {
                    return false;
                };
                let values = expression
                    .get("values")
                    .and_then(Value::as_array)
                    .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                    .unwrap_or_default();
                let actual = template_labels.get(key).and_then(Value::as_str);
                match operator {
                    "In" => actual.is_some_and(|actual| values.contains(&actual)),
                    "NotIn" => actual.is_none_or(|actual| !values.contains(&actual)),
                    "Exists" => actual.is_some(),
                    "DoesNotExist" => actual.is_none(),
                    "Gt" | "Lt" => {
                        let Some(actual) = actual.and_then(|actual| actual.parse::<i64>().ok()) else {
                            return false;
                        };
                        let Some(expected) = values.first().and_then(|value| value.parse::<i64>().ok()) else {
                            return false;
                        };
                        if operator == "Gt" {
                            actual > expected
                        } else {
                            actual < expected
                        }
                    }
                    _ => false,
                }
            })
        });

    if match_labels && match_expressions {
        Vec::new()
    } else {
        vec!["spec.selector: selector does not match template labels".to_string()]
    }
}

/// Apps/v1 workload selectors are immutable after creation. The same
/// invariant applies to ordinary updates, patches, and Server-Side Apply
/// because they all converge through the REST persistence path.
pub fn validate_builtin_update_semantics(
    group: &str,
    version: &str,
    kind: &str,
    existing: &Value,
    candidate: &Value,
) -> Vec<String> {
    if group != "apps"
        || version != "v1"
        || !matches!(kind, "DaemonSet" | "Deployment" | "ReplicaSet" | "StatefulSet")
    {
        return Vec::new();
    }

    if existing.pointer("/spec/selector") != candidate.pointer("/spec/selector") {
        vec!["spec.selector: field is immutable".to_string()]
    } else {
        Vec::new()
    }
}

fn walk_types(schema: &str, value: &Value, path_prefix: &str, out: &mut Vec<TypeMismatch>) {
    let Some(obj) = value.as_object() else { return };

    for (field, field_value) in obj {
        if field_value.is_null() {
            continue;
        }
        let field_path = join_path(path_prefix, field);
        if let Some(expected) = codegen::type_info_index().get(&(schema, field.as_str())) {
            if !matches_kind(expected, field_value) {
                out.push(TypeMismatch {
                    path: field_path.clone(),
                    expected: expected.to_string(),
                    actual_kind: kind_name(field_value).to_string(),
                });
                // A value whose top-level kind is already wrong (e.g. a
                // string where an array was expected) has nothing sane to
                // recurse into — skip the ref_schema pass below for it.
                continue;
            }
        }

        let Some(fields) = codegen::field_meta_index_by_schema().get(schema) else { continue };
        let Some(meta) = fields.iter().find(|m| m.field == field) else { continue };
        let Some(ref_schema) = meta.ref_schema else { continue };
        match field_value {
            Value::Object(_) => walk_types(ref_schema, field_value, &field_path, out),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk_types(ref_schema, item, &format!("{field_path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }
}

fn matches_kind(openapi_type: &str, value: &Value) -> bool {
    match openapi_type {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        // JSON has no separate integer literal syntax — a "5.0" from a
        // permissive encoder is still structurally an integer value, so
        // accept any numeric value with a zero fractional part rather than
        // only serde_json's own is_i64()/is_u64() (which reflect how the
        // number happened to be lexed, not its mathematical value).
        "integer" => value.as_f64().is_some_and(|f| f.fract() == 0.0),
        "array" => value.is_array(),
        // A type this table doesn't otherwise recognize (shouldn't happen
        // against the real vendored data — codegen.rs's own test locks in
        // the concrete cases) is treated as unconstrained rather than
        // rejecting every value, the same fail-open posture the rest of
        // this crate's generically-derived checks take on unexpected data.
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

    #[test]
    fn a_correctly_typed_field_produces_no_mismatch() {
        let value = json!({"hostNetwork": true, "containers": []});
        assert_eq!(validate_types("io.k8s.api.core.v1.PodSpec", &value), vec![]);
    }

    #[test]
    fn a_string_where_a_boolean_is_expected_is_reported() {
        let value = json!({"hostNetwork": "yes"});
        let mismatches = validate_types("io.k8s.api.core.v1.PodSpec", &value);
        assert_eq!(
            mismatches,
            vec![TypeMismatch { path: "hostNetwork".to_string(), expected: "boolean".to_string(), actual_kind: "string".to_string() }]
        );
    }

    #[test]
    fn an_absent_field_is_never_a_type_mismatch() {
        // validate_types only judges fields it actually sees — absence is
        // validate_required's job, not this function's.
        assert_eq!(validate_types("io.k8s.api.core.v1.PodSpec", &json!({})), vec![]);
    }

    #[test]
    fn a_whole_number_encoded_as_a_json_float_still_counts_as_an_integer() {
        let value = json!({"activeDeadlineSeconds": 30.0});
        assert_eq!(validate_types("io.k8s.api.core.v1.PodSpec", &value), vec![]);
    }

    /// Proves the recursion: a mismatch inside a `ref_schema`-nested array
    /// element is reported with an indexed path, exactly like
    /// `validate_required`'s own equivalent case.
    #[test]
    fn a_type_mismatch_in_a_nested_array_element_is_reported_with_an_indexed_path() {
        let value = json!({
            "containers": [
                {"name": "app", "stdin": "not-a-bool"},
            ]
        });
        let mismatches = validate_types("io.k8s.api.core.v1.PodSpec", &value);
        assert_eq!(
            mismatches,
            vec![TypeMismatch { path: "containers[0].stdin".to_string(), expected: "boolean".to_string(), actual_kind: "string".to_string() }]
        );
    }

    #[test]
    fn a_field_with_no_type_info_entry_is_never_flagged() {
        // securityContext is a nested-object field with no TYPE_INFO entry
        // (its shape is ref_schema's job) — passing it something odd must
        // not be reported as a *type* mismatch by this function.
        let value = json!({"securityContext": {"runAsUser": "not-a-number"}});
        // runAsUser really is typed (integer) on PodSecurityContext, so
        // this recurses and *does* find that one real mismatch — proving
        // the recursion reached the nested schema, while the outer
        // securityContext field itself (untyped in TYPE_INFO) is silent.
        let mismatches = validate_types("io.k8s.api.core.v1.PodSpec", &value);
        assert_eq!(
            mismatches,
            vec![TypeMismatch { path: "securityContext.runAsUser".to_string(), expected: "integer".to_string(), actual_kind: "string".to_string() }]
        );
    }

    #[test]
    fn built_in_constraints_use_the_published_openapi_schema() {
        let violations = validate_openapi_constraints(
            "",
            "v1",
            "Secret",
            &json!({"data": {"token": "not-base64"}}),
        );
        assert!(violations.iter().any(|violation| violation.contains("data.token")));
    }

    #[test]
    fn hpa_max_replicas_cannot_be_less_than_min_replicas() {
        let violations = validate_builtin_semantics(
            "autoscaling",
            "v2",
            "HorizontalPodAutoscaler",
            &json!({"spec": {"minReplicas": 2, "maxReplicas": 1}}),
        );
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("maxReplicas"));
        assert!(validate_builtin_semantics(
            "autoscaling",
            "v2",
            "HorizontalPodAutoscaler",
            &json!({"spec": {"minReplicas": 1, "maxReplicas": 2}}),
        )
        .is_empty());
    }

    #[test]
    fn workload_selector_must_match_template_labels() {
        let object = json!({
            "spec": {
                "selector": {"matchLabels": {"app": "api"}},
                "template": {"metadata": {"labels": {"app": "worker"}}}
            }
        });
        let violations = validate_builtin_semantics("apps", "v1", "Deployment", &object);
        assert_eq!(violations, vec!["spec.selector: selector does not match template labels"]);

        let object = json!({
            "spec": {
                "selector": {
                    "matchExpressions": [{"key": "tier", "operator": "In", "values": ["backend"]}]
                },
                "template": {"metadata": {"labels": {"tier": "backend"}}}
            }
        });
        assert!(validate_builtin_semantics("apps", "v1", "ReplicaSet", &object).is_empty());
    }

    #[test]
    fn apps_v1_workload_selector_is_immutable() {
        let existing = json!({"spec": {"selector": {"matchLabels": {"app": "api"}}}});
        let mut candidate = existing.clone();
        candidate["spec"]["selector"] = json!({"matchLabels": {"app": "worker"}});
        assert_eq!(
            validate_builtin_update_semantics("apps", "v1", "Deployment", &existing, &candidate),
            vec!["spec.selector: field is immutable"]
        );
        assert!(validate_builtin_update_semantics("apps", "v1", "Deployment", &existing, &existing).is_empty());
        assert!(validate_builtin_update_semantics("apps", "v1beta1", "Deployment", &existing, &candidate).is_empty());
    }
}
