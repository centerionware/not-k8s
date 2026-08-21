//! CEL Phase 4: real *runtime* evaluation of `x-kubernetes-validations`
//! rules against an actual custom resource instance — distinct from
//! [`super::cel_validations`] (the CRD-acceptance-time *cost* check on
//! the rules themselves): this module actually *runs* them, real
//! upstream's own `customResourceStrategy.Validate`/`ValidateUpdate`
//! (`k8s.io/apiextensions-apiserver/pkg/registry/customresource/
//! strategy.go`, confirmed directly — see [`super::cel_validations`]'s
//! own doc comment for exactly where `RuntimeCELCostBudget` vs.
//! `StaticEstimatedCostLimit` each apply) minus that function's own many
//! other real concerns (defaulting, pruning, structural validation —
//! each already its own module, not duplicated here).
//!
//! [`validate_object`] recursively walks a CRD version's own schema
//! *and* the real object together (the same data-driven recursion
//! `apiextensions::schema_validation`'s own `walk_required`/`walk_types`
//! already establish — only descend where the object actually has
//! data), evaluating every `x-kubernetes-validations` rule found at any
//! level against that level's own real value, `self` bound to it and
//! `oldSelf` to the corresponding value in `old_value` (`None` on
//! `CREATE` — real upstream's own rule: `oldSelf` is genuinely
//! unavailable then, not just empty).
//!
//! **Named, honest scope**: each rule gets its own [`crate::cel_ext::
//! eval_bool_with_deadline`] call, deadline [`PER_RULE_DEADLINE`] (real
//! upstream's own `PerCallLimit`, ~0.1s — already documented in
//! `docs/APISERVER.md`'s own `cel_ext` section, this module's own real
//! per-rule use of that same wall-clock stand-in). Real upstream's own
//! *aggregate* `RuntimeCELCostBudget` across every rule evaluated for
//! one object is **not enforced here** — this crate has no real
//! runtime cost-accounting mechanism at all (`cel_ext`'s own top-level
//! doc comment names this as Phase 2's still-not-closed gap), so a
//! schema with very many rules could in principle run for the sum of
//! all their own individual deadlines rather than one shared ~1s
//! ceiling; a real, separate follow-up once real cost tracking exists,
//! not silently assumed away.

use crate::cel_ext::eval_bool_with_deadline;
use serde_json::Value;
use std::time::Duration;

/// Real upstream's own `PerCallLimit`, in wall-clock terms — see this
/// module's own doc comment.
const PER_RULE_DEADLINE: Duration = Duration::from_millis(100);

/// One `x-kubernetes-validations` rule that failed against a real
/// object — either it evaluated `false` (a real validation failure,
/// `message` is the rule's own declared message or a generic fallback)
/// or it errored (a compile/runtime failure — real upstream's own
/// posture: an unevaluable rule is itself a validation failure, not
/// silently skipped).
#[derive(Debug, Clone, PartialEq)]
pub struct RuleViolation {
    pub path: String,
    pub rule_index: usize,
    pub message: String,
}

impl std::fmt::Display for RuleViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let field = if self.path.is_empty() { String::new() } else { format!("{}.", self.path) };
        write!(f, "{field}x-kubernetes-validations[{}].rule: {}", self.rule_index, self.message)
    }
}

/// Real upstream's own runtime rule evaluation — see this module's own
/// doc comment for the exact real scope.
pub fn validate_object(schema: &Value, value: &Value, old_value: Option<&Value>) -> Vec<RuleViolation> {
    let mut out = Vec::new();
    walk(schema, value, old_value, "", &mut out);
    out
}

fn walk(schema: &Value, value: &Value, old_value: Option<&Value>, path: &str, out: &mut Vec<RuleViolation>) {
    if let Some(rules) = schema.get("x-kubernetes-validations").and_then(Value::as_array) {
        for (i, rule) in rules.iter().enumerate() {
            let Some(rule_str) = rule.get("rule").and_then(Value::as_str) else { continue };
            match eval_bool_with_deadline(rule_str, value, old_value, PER_RULE_DEADLINE) {
                Ok(true) => {}
                Ok(false) => {
                    let message = rule.get("message").and_then(Value::as_str).unwrap_or("failed validation").to_string();
                    out.push(RuleViolation { path: path.to_string(), rule_index: i, message });
                }
                Err(e) => out.push(RuleViolation { path: path.to_string(), rule_index: i, message: format!("rule evaluation failed: {e}") }),
            }
        }
    }

    let Some(obj) = value.as_object() else { return };

    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, prop_schema) in properties {
            let Some(current) = obj.get(name) else { continue };
            let old_current = old_value.and_then(|o| o.get(name));
            walk(prop_schema, current, old_current, &join_path(path, name), out);
        }
    }
    if let Some(items_schema) = schema.get("items") {
        if let Some(items) = value.as_array() {
            let old_items = old_value.and_then(Value::as_array);
            for (i, item) in items.iter().enumerate() {
                let old_item = old_items.and_then(|o| o.get(i));
                walk(items_schema, item, old_item, &format!("{path}[{i}]"), out);
            }
        }
    }
    // A real nested schema only, never the boolean `additionalProperties:
    // true/false` shorthand -- same distinction `cel_ext::decl_type`'s
    // own conversion already draws for the same reason.
    if let Some(additional) = schema.get("additionalProperties").filter(|a| a.is_object()) {
        for (key, val) in obj {
            let old_val = old_value.and_then(|o| o.get(key));
            walk(additional, val, old_val, &format!("{path}[{key}]"), out);
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
    fn a_schema_with_no_rules_at_all_has_no_violations() {
        let schema = json!({"type": "object", "properties": {"name": {"type": "string"}}});
        let value = json!({"name": "x"});
        assert!(validate_object(&schema, &value, None).is_empty());
    }

    #[test]
    fn a_passing_rule_at_the_root_has_no_violations() {
        let schema = json!({
            "type": "object",
            "properties": {"replicas": {"type": "integer"}},
            "x-kubernetes-validations": [{"rule": "self.replicas > 0"}],
        });
        let value = json!({"replicas": 3});
        assert!(validate_object(&schema, &value, None).is_empty());
    }

    #[test]
    fn a_failing_rule_reports_its_own_declared_message() {
        let schema = json!({
            "type": "object",
            "properties": {"replicas": {"type": "integer"}},
            "x-kubernetes-validations": [{"rule": "self.replicas > 0", "message": "replicas must be positive"}],
        });
        let value = json!({"replicas": -1});
        let violations = validate_object(&schema, &value, None);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].message, "replicas must be positive");
        assert_eq!(violations[0].rule_index, 0);
    }

    #[test]
    fn a_failing_rule_with_no_declared_message_gets_a_real_fallback() {
        let schema = json!({
            "type": "object",
            "properties": {"replicas": {"type": "integer"}},
            "x-kubernetes-validations": [{"rule": "self.replicas > 0"}],
        });
        let value = json!({"replicas": -1});
        let violations = validate_object(&schema, &value, None);
        assert_eq!(violations[0].message, "failed validation");
    }

    #[test]
    fn a_rule_using_old_self_sees_the_real_old_value_on_update() {
        let schema = json!({
            "type": "object",
            "properties": {"replicas": {"type": "integer"}},
            "x-kubernetes-validations": [{"rule": "self.replicas >= oldSelf.replicas", "message": "replicas may not decrease"}],
        });
        let old_value = json!({"replicas": 5});
        let increased = json!({"replicas": 6});
        let decreased = json!({"replicas": 4});
        assert!(validate_object(&schema, &increased, Some(&old_value)).is_empty());
        let violations = validate_object(&schema, &decreased, Some(&old_value));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].message, "replicas may not decrease");
    }

    #[test]
    fn a_rule_nested_under_a_property_is_evaluated_against_that_propertys_own_value() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "x-kubernetes-validations": [{"rule": "self.name != ''", "message": "name must not be empty"}],
                },
            },
        });
        let value = json!({"spec": {"name": ""}});
        let violations = validate_object(&schema, &value, None);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].path, "spec");
        assert_eq!(violations[0].message, "name must not be empty");
    }

    #[test]
    fn a_rule_nested_under_array_items_is_evaluated_once_per_real_element() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tags": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "x-kubernetes-validations": [{"rule": "self != ''", "message": "tag must not be empty"}],
                    },
                },
            },
        });
        let value = json!({"tags": ["ok", "", "also-ok"]});
        let violations = validate_object(&schema, &value, None);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].path, "tags[1]");
    }

    #[test]
    fn a_rule_that_fails_to_compile_is_reported_not_silently_skipped() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [{"rule": "this is not valid cel((("}],
        });
        let violations = validate_object(&schema, &json!({}), None);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].message.contains("rule evaluation failed"));
    }

    #[test]
    fn display_formats_a_real_readable_violation_path() {
        let v = RuleViolation { path: "spec".to_string(), rule_index: 2, message: "bad".to_string() };
        assert_eq!(v.to_string(), "spec.x-kubernetes-validations[2].rule: bad");
    }

    #[test]
    fn display_omits_the_leading_dot_at_the_schema_root() {
        let v = RuleViolation { path: String::new(), rule_index: 0, message: "bad".to_string() };
        assert_eq!(v.to_string(), "x-kubernetes-validations[0].rule: bad");
    }
}
