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
//! Each rule gets its own [`crate::cel_ext::eval_bool_with_deadline`] call,
//! with the smaller of the real upstream `PerCallLimit` (~0.1s) and the
//! remaining shared `RuntimeCELCostBudget` (~1s) for this object. The
//! underlying CEL crate exposes no interpreter fuel/step hook, so the
//! shared budget is enforced as a wall-clock deadline: it bounds the
//! request-side evaluation window and stops walking further rules once
//! that window is exhausted. A timed-out evaluation thread can still
//! finish in the background because Rust cannot safely cancel arbitrary
//! threads; the request concurrency gate limits how many such evaluations
//! can be started at once.

use crate::cel_ext::{eval_bool_with_deadline, eval_bool_with_optional_old_self_and_deadline, Error as CelError};
use crate::cel_ext::type_check::rule_references_old_self;
use serde_json::Value;
use std::cmp::min;
use std::time::Duration;
use std::time::Instant;

/// Real upstream's own `PerCallLimit`, in wall-clock terms — see this
/// module's own doc comment.
const PER_RULE_DEADLINE: Duration = Duration::from_millis(100);

/// Real upstream's own `RuntimeCELCostBudget`, represented by a shared
/// wall-clock budget because the CEL interpreter used here has no exposed
/// cost or interruption hook.
const RUNTIME_CEL_BUDGET: Duration = Duration::from_secs(1);

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
    validate_object_with_budget(schema, value, old_value, RUNTIME_CEL_BUDGET)
}

fn validate_object_with_budget(schema: &Value, value: &Value, old_value: Option<&Value>, budget: Duration) -> Vec<RuleViolation> {
    let mut out = Vec::new();
    let deadline = Instant::now() + budget;
    let mut budget_exhausted = false;
    walk(schema, value, old_value, "", deadline, &mut budget_exhausted, &mut out);
    out
}

fn walk(schema: &Value, value: &Value, old_value: Option<&Value>, path: &str, deadline: Instant, budget_exhausted: &mut bool, out: &mut Vec<RuleViolation>) {
    if *budget_exhausted {
        return;
    }
    if let Some(rules) = schema.get("x-kubernetes-validations").and_then(Value::as_array) {
        for (i, rule) in rules.iter().enumerate() {
            let Some(rule_str) = rule.get("rule").and_then(Value::as_str) else { continue };
            let optional_old_self = rule.get("optionalOldSelf").and_then(Value::as_bool).unwrap_or(false);
            if !optional_old_self && old_value.is_none() && rule_references_old_self(rule_str) {
                // Transition rules are evaluated on UPDATE only. A missing
                // old value is not a runtime error unless the rule opts into
                // the optional-old-self form.
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                out.push(RuleViolation { path: path.to_string(), rule_index: i, message: "runtime CEL cost budget exceeded".to_string() });
                *budget_exhausted = true;
                return;
            }
            let rule_result = if optional_old_self {
                eval_bool_with_optional_old_self_and_deadline(rule_str, value, old_value, min(PER_RULE_DEADLINE, remaining))
            } else {
                eval_bool_with_deadline(rule_str, value, old_value, min(PER_RULE_DEADLINE, remaining))
            };
            match rule_result {
                Ok(true) => {}
                Ok(false) => {
                    let message = rule.get("message").and_then(Value::as_str).unwrap_or("failed validation").to_string();
                    out.push(RuleViolation { path: path.to_string(), rule_index: i, message });
                }
                Err(e) => {
                    let deadline_hit = matches!(&e, CelError::DeadlineExceeded);
                    out.push(RuleViolation { path: path.to_string(), rule_index: i, message: format!("rule evaluation failed: {e}") });
                    if deadline_hit {
                        *budget_exhausted = true;
                        return;
                    }
                }
            }
        }
    }

    if Instant::now() >= deadline {
        *budget_exhausted = true;
        return;
    }

    // Real upstream's own three real container shapes -- `properties`/
    // `additionalProperties` only make sense to descend when `value` is
    // itself an object, `items` only when it's an array. **Real bug
    // caught by CI, fixed same PR**: an earlier draft gated all three
    // behind one shared `value.as_object()` binding, which silently made
    // `items` unreachable for every real array value (a JSON array's own
    // `.as_object()` is always `None`) — no test in this file ever
    // recursed into a real array until the one that caught it.
    if let Some(obj) = value.as_object() {
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, prop_schema) in properties {
                let Some(current) = obj.get(name) else { continue };
                let old_current = old_value.and_then(|o| o.get(name));
                walk(prop_schema, current, old_current, &join_path(path, name), deadline, budget_exhausted, out);
                if *budget_exhausted { return; }
            }
        }
        // A real nested schema only, never the boolean
        // `additionalProperties: true/false` shorthand -- same
        // distinction `cel_ext::decl_type`'s own conversion already
        // draws for the same reason.
        if let Some(additional) = schema.get("additionalProperties").filter(|a| a.is_object()) {
            for (key, val) in obj {
                let old_val = old_value.and_then(|o| o.get(key));
                walk(additional, val, old_val, &format!("{path}[{key}]"), deadline, budget_exhausted, out);
                if *budget_exhausted { return; }
            }
        }
    }
    if let Some(items_schema) = schema.get("items") {
        if let Some(items) = value.as_array() {
            let old_items = old_value.and_then(Value::as_array);
            for (i, item) in items.iter().enumerate() {
                let old_item = old_items.and_then(|o| o.get(i));
                walk(items_schema, item, old_item, &format!("{path}[{i}]"), deadline, budget_exhausted, out);
                if *budget_exhausted { return; }
            }
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
        let created = json!({"replicas": 1});
        assert!(validate_object(&schema, &created, None).is_empty());
        let old_value = json!({"replicas": 5});
        let increased = json!({"replicas": 6});
        let decreased = json!({"replicas": 4});
        assert!(validate_object(&schema, &increased, Some(&old_value)).is_empty());
        let violations = validate_object(&schema, &decreased, Some(&old_value));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].message, "replicas may not decrease");
    }

    #[test]
    fn an_optional_old_self_rule_handles_create_and_update() {
        let schema = json!({
            "type": "object",
            "properties": {"replicas": {"type": "integer"}},
            "x-kubernetes-validations": [{
                "rule": "oldSelf.hasValue() ? oldSelf.value().replicas <= self.replicas : true",
                "optionalOldSelf": true,
                "message": "replicas may not decrease",
            }],
        });
        let created = json!({"replicas": 1});
        assert!(validate_object(&schema, &created, None).is_empty());

        let old_value = json!({"replicas": 2});
        let increased = json!({"replicas": 3});
        assert!(validate_object(&schema, &increased, Some(&old_value)).is_empty());

        let decreased = json!({"replicas": 1});
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
    fn the_shared_runtime_budget_stops_evaluation_before_the_first_rule_when_exhausted() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [{"rule": "true"}],
        });
        let violations = validate_object_with_budget(&schema, &json!({}), None, Duration::ZERO);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].message, "runtime CEL cost budget exceeded");
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
