//! Wires `cel_ext::budget`'s real per-rule cost check into the actual
//! CRD-acceptance path — the piece `cel_ext::budget`'s own module doc
//! comment named as still missing: "finding where in `apiextensions`'s
//! own CRD-establishing flow to call this from."
//!
//! [`validate_schema_cel_costs`] is the real recursive walk (a faithful,
//! schema-only port of real upstream's own `ValidateCustomResourceDefinitionOpenAPISchema`,
//! `pkg/apis/apiextensions/validation/validation.go`, fetched and read
//! directly, scoped to the CEL-cost half of what that function checks —
//! its own many other structural checks are `apiextensions::
//! schema_validation`'s job, not duplicated here): at every schema level
//! that declares its own `x-kubernetes-validations` rules — not just the
//! root, a rule can live on any nested `properties`/`items`/
//! `additionalProperties` schema, evaluated with `self` bound to
//! *that* level's own value, not the whole object — each rule's real
//! cost is checked against the rule's own schema cost multiplied by the
//! maximum number of enclosing array elements/map entries that can reach
//! that level, using a `DeclType` built fresh from that same nested schema
//! level (never the root's), since a rule's own `self` genuinely means
//! "the value here," not "the value at the top."
//!
//! [`validate_crd_cel_costs`] is the actual `CustomResourceDefinition`
//! object wiring: walks every declared `spec.versions[]`'s own
//! `schema.openAPIV3Schema`, turning each real violation into the same
//! `Vec<String>` shape `server::rest::create`/`update`'s own
//! `violations` accumulator already expects (the same convention
//! `name_format_violations`/`schema_validation`'s own callers use).
//! **Wired into both**: a `CustomResourceDefinition` `CREATE`/`UPDATE`
//! whose schema declares a rule too expensive to run gets a real `422`,
//! the same `Invalid` outcome every other structural violation already
//! produces — a client authoring a runaway `x-kubernetes-validations`
//! rule now finds out at CRD-acceptance time, not the first time some
//! real custom resource instance trips it at runtime.

use crate::cel_ext::budget::{check_rule_cost_with_cardinality, RuleCostError};
use crate::cel_ext::decl_type;
use crate::cel_ext::type_check::{check_root_rule, check_rule, TypeError};
use serde_json::Value;

/// One `x-kubernetes-validations` rule whose real cost exceeds budget —
/// `path` is the schema location the rule is declared on (empty string
/// for the schema root), `rule_index` its own position within that
/// level's `x-kubernetes-validations` array.
#[derive(Debug, Clone, PartialEq)]
pub struct CelCostViolation {
    pub path: String,
    pub rule_index: usize,
    pub error: RuleCostError,
}

/// Real upstream's own recursive schema walk, scoped to the CEL-cost
/// check alone — see this module's own doc comment. Schema-only, no
/// real object instance involved (real upstream's own timing: this runs
/// once, when the CRD itself is accepted, long before any actual custom
/// resource using it exists).
pub fn validate_schema_cel_costs(schema: &Value) -> Vec<CelCostViolation> {
    let mut out = Vec::new();
    walk(schema, "", 1, &mut out);
    out
}

/// One schema-scoped CEL rule that cannot be type-checked against the
/// structural schema at that location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CelTypeViolation {
    pub path: String,
    pub rule_index: usize,
    pub error: TypeError,
}

/// Run the schema-aware part of Kubernetes' CRD CEL type checker. Syntax
/// errors are already reported by [`validate_schema_cel_costs`], so this
/// function returns only semantic type errors and avoids duplicate messages
/// for one malformed rule.
pub fn validate_schema_cel_types(schema: &Value) -> Vec<CelTypeViolation> {
    let mut out = Vec::new();
    walk_types(schema, "", &mut out);
    out
}

fn walk_types(schema: &Value, path: &str, out: &mut Vec<CelTypeViolation>) {
    if let Some(rules) = schema.get("x-kubernetes-validations").and_then(Value::as_array) {
        for (i, rule) in rules.iter().enumerate() {
            let Some(rule_str) = rule.get("rule").and_then(Value::as_str) else { continue };
            let errors = if path.is_empty() { check_root_rule(schema, rule_str) } else { check_rule(schema, rule_str) };
            for error in errors.into_iter().filter(|error| !matches!(error, TypeError::Compile(_))) {
                out.push(CelTypeViolation { path: path.to_string(), rule_index: i, error });
            }
        }
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, prop_schema) in properties {
            walk_types(prop_schema, &join_path(path, name), out);
        }
    }
    if let Some(items) = schema.get("items") {
        walk_types(items, &format!("{path}[*]"), out);
    }
    if let Some(additional) = schema.get("additionalProperties").filter(|value| value.is_object()) {
        walk_types(additional, &format!("{path}[*]"), out);
    }
}

fn walk(schema: &Value, path: &str, cardinality: u64, out: &mut Vec<CelCostViolation>) {
    if let Some(rules) = schema.get("x-kubernetes-validations").and_then(Value::as_array) {
        // A schema that declares rules must itself convert to a real
        // DeclType to check them against. When it can't (a shape
        // `decl_type_for` declines to expose — that function's own doc
        // comment), that's a genuine structural problem
        // `apiextensions::schema_validation`'s own checks are
        // responsible for surfacing, not this module's job to
        // re-report — real upstream's own posture for this exact case
        // ("skip CEL expression validation" once schema errors already
        // exist) is mirrored here by simply not checking cost either.
        if let Some(root) = decl_type::decl_type_for(schema) {
            for (i, rule) in rules.iter().enumerate() {
                let Some(rule_str) = rule.get("rule").and_then(Value::as_str) else { continue };
                if let Err(error) = check_rule_cost_with_cardinality(&root, rule_str, cardinality) {
                    out.push(CelCostViolation { path: path.to_string(), rule_index: i, error });
                }
            }
        }
    }

    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, prop_schema) in properties {
            walk(prop_schema, &join_path(path, name), cardinality, out);
        }
    }
    if let Some(items) = schema.get("items") {
        let child_cardinality = cardinality.saturating_mul(max_collection_cardinality(schema, "maxItems"));
        walk(items, &format!("{path}[*]"), child_cardinality, out);
    }
    // Only a real nested schema, never the boolean `additionalProperties:
    // true/false` shorthand -- same distinction `decl_type::decl_type_for`
    // itself already draws for exactly the same reason.
    if let Some(additional) = schema.get("additionalProperties").filter(|a| a.is_object()) {
        let child_cardinality = cardinality.saturating_mul(max_collection_cardinality(schema, "maxProperties"));
        walk(additional, &format!("{path}[*]"), child_cardinality, out);
    }
}

/// Return the maximum number of children a collection schema can expose.
/// Explicit bounds are preferred; otherwise use the same request-size-derived
/// bound as `decl_type_for`. An unrepresentable/invalid shape stays
/// conservative so a nested rule is never silently under-costed.
fn max_collection_cardinality(schema: &Value, keyword: &str) -> u64 {
    if let Some(value) = schema.get(keyword).and_then(Value::as_i64) {
        return value.max(0) as u64;
    }
    decl_type::decl_type_for(schema).map(|decl| decl.max_elements.max(0) as u64).unwrap_or(u64::MAX)
}

fn join_path(prefix: &str, field: &str) -> String {
    if prefix.is_empty() {
        field.to_string()
    } else {
        format!("{prefix}.{field}")
    }
}

fn describe(error: &RuleCostError) -> String {
    match error {
        RuleCostError::Compile(detail) => format!("compilation failed: {detail}"),
        RuleCostError::TooExpensive { estimated_cost, limit } => {
            format!("estimated rule cost {estimated_cost} exceeds budget by a factor of {:.1}x (limit {limit})", *estimated_cost as f64 / *limit as f64)
        }
    }
}

/// The actual `CustomResourceDefinition` object wiring — see this
/// module's own doc comment.
pub fn validate_crd_cel_costs(crd: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(versions) = crd.pointer("/spec/versions").and_then(Value::as_array) else {
        return out;
    };
    for version in versions {
        let version_name = version.get("name").and_then(Value::as_str).unwrap_or("");
        let Some(schema) = version.pointer("/schema/openAPIV3Schema") else { continue };
        for violation in validate_schema_cel_costs(schema) {
            let field = if violation.path.is_empty() { String::new() } else { format!(".{}", violation.path) };
            out.push(format!("spec.versions[{version_name}].schema.openAPIV3Schema{field}.x-kubernetes-validations[{}].rule: {}", violation.rule_index, describe(&violation.error)));
        }
    }
    out
}

/// Validate the CEL types in every served CRD version and format the
/// findings for the generic CRD `Invalid` response.
pub fn validate_crd_cel_types(crd: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(versions) = crd.pointer("/spec/versions").and_then(Value::as_array) else {
        return out;
    };
    for version in versions {
        let version_name = version.get("name").and_then(Value::as_str).unwrap_or("");
        let Some(schema) = version.pointer("/schema/openAPIV3Schema") else { continue };
        for violation in validate_schema_cel_types(schema) {
            let field = if violation.path.is_empty() { String::new() } else { format!(".{}", violation.path) };
            out.push(format!("spec.versions[{version_name}].schema.openAPIV3Schema{field}.x-kubernetes-validations[{}].rule: {}", violation.rule_index, violation.error));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cel_ext::budget::STATIC_ESTIMATED_COST_LIMIT;
    use serde_json::json;

    #[test]
    fn a_schema_with_no_rules_at_all_has_no_violations() {
        let schema = json!({"type": "object", "properties": {"name": {"type": "string"}}});
        assert!(validate_schema_cel_costs(&schema).is_empty());
    }

    #[test]
    fn an_undeclared_field_is_rejected_by_the_schema_type_check() {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "x-kubernetes-validations": [{"rule": "self.missing == 'x'"}],
        });
        let violations = validate_schema_cel_types(&schema);
        assert!(violations.iter().any(|violation| matches!(&violation.error, TypeError::UnknownField { field, .. } if field == "missing")));
    }

    #[test]
    fn a_non_boolean_rule_is_rejected_at_crd_acceptance_time() {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "x-kubernetes-validations": [{"rule": "self.name"}],
        });
        let violations = validate_schema_cel_types(&schema);
        assert!(violations.iter().any(|violation| matches!(violation.error, TypeError::NonBoolean(_))));
    }

    #[test]
    fn a_nested_rule_is_checked_against_its_own_schema_scope() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {"replicas": {"type": "integer"}},
                    "x-kubernetes-validations": [{"rule": "self.replicas > 0"}],
                },
            },
        });
        assert!(validate_schema_cel_types(&schema).is_empty());
    }

    #[test]
    fn a_cheap_rule_at_the_root_has_no_violations() {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string", "maxLength": 10}},
            "x-kubernetes-validations": [{"rule": "self.name == 'x'"}],
        });
        assert!(validate_schema_cel_costs(&schema).is_empty());
    }

    #[test]
    fn a_too_expensive_rule_at_the_root_is_reported_with_its_own_index() {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "x-kubernetes-validations": [{"rule": "self.unknownField.matches('a+')"}],
        });
        let violations = validate_schema_cel_costs(&schema);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].path, "");
        assert_eq!(violations[0].rule_index, 0);
        assert!(matches!(violations[0].error, RuleCostError::TooExpensive { .. }));
    }

    #[test]
    fn a_rule_nested_under_a_property_is_checked_against_that_propertys_own_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {"replicas": {"type": "integer"}},
                    "x-kubernetes-validations": [{"rule": "self.replicas > 0"}],
                },
            },
        });
        let violations = validate_schema_cel_costs(&schema);
        assert!(violations.is_empty(), "a cheap rule nested under spec should still pass, got {violations:?}");
    }

    #[test]
    fn a_rule_nested_under_a_property_reports_that_propertys_own_path() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "x-kubernetes-validations": [{"rule": "self.unknownField.matches('a+')"}],
                },
            },
        });
        let violations = validate_schema_cel_costs(&schema);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].path, "spec");
    }

    #[test]
    fn a_rule_nested_under_array_items_is_found() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tags": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "x-kubernetes-validations": [{"rule": "self.unknownField.matches('a+')"}],
                    },
                },
            },
        });
        let violations = validate_schema_cel_costs(&schema);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].path, "tags[*]");
    }

    #[test]
    fn a_nested_rule_cost_includes_each_enclosing_array_element() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "maxItems": 100,
                    "items": {
                        "type": "object",
                        "properties": {"name": {"type": "string", "maxLength": 1_000_000}},
                        "x-kubernetes-validations": [{"rule": "self.name.matches('a+')"}],
                    },
                },
            },
        });
        let violations = validate_schema_cel_costs(&schema);
        assert_eq!(violations.len(), 1, "expected the ancestor array bound to make the nested rule too expensive: {violations:?}");
        assert_eq!(violations[0].path, "items[*]");
        assert!(matches!(violations[0].error, RuleCostError::TooExpensive { .. }));
    }

    #[test]
    fn an_unparseable_rule_is_reported_as_a_compile_error_not_silently_skipped() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [{"rule": "this is not valid cel((("}],
        });
        let violations = validate_schema_cel_costs(&schema);
        assert_eq!(violations.len(), 1);
        assert!(matches!(violations[0].error, RuleCostError::Compile(_)));
    }

    #[test]
    fn validate_crd_cel_costs_walks_every_declared_version() {
        let crd = json!({
            "spec": {
                "versions": [
                    {
                        "name": "v1",
                        "schema": {"openAPIV3Schema": {
                            "type": "object",
                            "x-kubernetes-validations": [{"rule": "self.unknownField.matches('a+')"}],
                        }},
                    },
                    {
                        "name": "v2",
                        "schema": {"openAPIV3Schema": {"type": "object"}},
                    },
                ],
            },
        });
        let messages = validate_crd_cel_costs(&crd);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("v1"), "expected the violating version's own name in the message: {messages:?}");
        assert!(messages[0].contains("x-kubernetes-validations[0].rule"));
    }

    #[test]
    fn validate_crd_cel_costs_is_empty_for_a_crd_with_no_versions_at_all() {
        assert!(validate_crd_cel_costs(&json!({})).is_empty());
    }

    #[test]
    fn describe_names_the_real_budget_limit() {
        let msg = describe(&RuleCostError::TooExpensive { estimated_cost: 20_000_000, limit: STATIC_ESTIMATED_COST_LIMIT });
        assert!(msg.contains("20000000"));
        assert!(msg.contains(&STATIC_ESTIMATED_COST_LIMIT.to_string()));
    }
}
