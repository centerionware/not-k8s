//! `ValidatingAdmissionPolicy`'s own `spec.validations[]` evaluation —
//! real upstream's own `validator.Validate`
//! (`k8s.io/apiserver/pkg/admission/plugin/policy/validating/
//! validator.go`, fetched and read directly), scoped to the pure evaluation
//! part of that function. The storage-backed policy adapter supplies the
//! real binding/`paramRef` and audit-annotation wiring around this primitive;
//! this module only evaluates the already-bound inputs:
//! given an already-bound `object`/`oldObject`/`request`/`params`
//! variable set (the same shape [`super::policy_matching::
//! build_request_object`] and `cel_ext::eval_bool_with_vars` already
//! establish) and a policy's own declared `spec.validations`, decide
//! `Admit`/`Deny` per rule with the real message and reason real upstream
//! would report.
//!
//! **Unlike [`super::match_conditions`]**: real upstream evaluates every
//! validation, never short-circuiting on the first failure — each of a
//! policy's own `spec.validations` entries produces its own independent
//! [`Decision`], since a real caller (an eventual `ValidatingAdmissionPolicy
//! Binding`'s own `validationActions`) needs to see every violation, not
//! just the first.
//!
//! **Real upstream's own message-resolution order**, ported exactly from
//! `Validate`'s own real logic: a real `false` result first tries
//! `messageExpression` (only if it evaluates to a non-empty, single-line,
//! at-most-[`MAX_MESSAGE_EXPRESSION_BYTES`]-byte string — any other
//! outcome, including a real evaluation error, is silently discarded, not
//! surfaced, matching real upstream's own `klog`-only-log posture for a
//! broken `messageExpression`), then falls back to the rule's own declared
//! `message`, then to a generic `"failed expression: <rule>"`. `reason`
//! defaults to real upstream's own `metav1.StatusReasonInvalid` (`"Invalid"`)
//! when the rule doesn't declare its own.
//!
//! A compile/evaluation *error* on the boolean expression itself (not a
//! real `false` result) is governed by the policy's own `failurePolicy`,
//! same real distinction [`super::match_conditions`] already draws:
//! `Fail` denies, `Ignore` admits — either way [`Decision::is_error`]
//! stays `true` so a caller can still tell the two apart, matching real
//! upstream's own `Evaluation: EvalError` vs. `EvalDeny`/`EvalAdmit`.
//!
//! The storage-backed policy adapter now consumes this pure decision
//! primitive for real `ValidatingAdmissionPolicy` requests.

use super::match_conditions::FailurePolicy;
use crate::cel_ext::{eval_bool_with_vars_and_cel_vars_and_deadline, eval_string_with_vars_and_deadline};
use serde_json::Value;
use std::time::Duration;

/// Real upstream's own `PerCallLimit`-derived wall-clock stand-in — the
/// same `~0.1s` bound every other CEL rule evaluation in this crate uses
/// (`apiextensions::cel_evaluate`, `match_conditions`).
const PER_VALIDATION_DEADLINE: Duration = Duration::from_millis(100);

/// Real upstream's own `celconfig.MaxEvaluatedMessageExpressionSizeBytes`
/// (`k8s.io/apiserver/pkg/apis/cel/config.go`, fetched and read directly).
const MAX_MESSAGE_EXPRESSION_BYTES: usize = 5 * 1024;

/// Real upstream's own default `Reason` — `metav1.StatusReasonInvalid`.
const DEFAULT_REASON: &str = "Invalid";

/// One real `v1.Validation` entry.
#[derive(Debug, Clone, Copy)]
pub struct Validation<'a> {
    pub expression: &'a str,
    pub message: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub message_expression: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Admit,
    Deny,
}

/// Real upstream's own `PolicyDecision`, scoped to the fields this
/// primitive can honestly produce (no `Elapsed`, no
/// `PolicyAuditAnnotation` (the storage-backed policy enforcement layer
/// adds that annotation around this pure result).
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: Action,
    /// `true` when this decision came from a compile/evaluation error on
    /// the rule's own boolean expression, not a real `false` result —
    /// real upstream's own `Evaluation: EvalError` distinction.
    pub is_error: bool,
    pub message: Option<String>,
    pub reason: Option<String>,
}

/// Evaluates every `validations` entry independently against the same
/// `vars` — see this module's own doc comment for why this never
/// short-circuits, unlike [`super::match_conditions::match_conditions`].
pub fn evaluate_validations(validations: &[Validation], vars: &[(&'static str, &Value)], failure_policy: FailurePolicy) -> Vec<Decision> {
    evaluate_validations_with_cel_vars(validations, vars, &[], failure_policy)
}

/// [`evaluate_validations`] with native CEL values in addition to JSON
/// values. The extra values are used for opaque Kubernetes CEL bindings.
pub fn evaluate_validations_with_cel_vars(validations: &[Validation], vars: &[(&'static str, &Value)], cel_vars: &[(&'static str, cel::Value)], failure_policy: FailurePolicy) -> Vec<Decision> {
    validations.iter().map(|v| evaluate_validation_with_cel_vars(v, vars, cel_vars, failure_policy)).collect()
}

fn evaluate_validation_with_cel_vars(v: &Validation, vars: &[(&'static str, &Value)], cel_vars: &[(&'static str, cel::Value)], failure_policy: FailurePolicy) -> Decision {
    match eval_bool_with_vars_and_cel_vars_and_deadline(v.expression, vars, cel_vars, PER_VALIDATION_DEADLINE) {
        Err(e) => Decision { action: error_action(failure_policy), is_error: true, message: Some(e.to_string()), reason: None },
        Ok(true) => Decision { action: Action::Admit, is_error: false, message: None, reason: None },
        Ok(false) => Decision {
            action: Action::Deny,
            is_error: false,
            message: Some(resolve_message(v, vars)),
            reason: Some(v.reason.unwrap_or(DEFAULT_REASON).to_string()),
        },
    }
}

fn error_action(failure_policy: FailurePolicy) -> Action {
    match failure_policy {
        FailurePolicy::Fail => Action::Deny,
        FailurePolicy::Ignore => Action::Admit,
    }
}

/// Real upstream's own real message-resolution order — see this module's
/// own doc comment.
fn resolve_message(v: &Validation, vars: &[(&'static str, &Value)]) -> String {
    if let Some(expr) = v.message_expression {
        if let Ok(evaluated) = eval_string_with_vars_and_deadline(expr, vars, PER_VALIDATION_DEADLINE) {
            let trimmed = evaluated.trim();
            if !trimmed.is_empty() && trimmed.len() <= MAX_MESSAGE_EXPRESSION_BYTES && !trimmed.contains('\n') {
                return trimmed.to_string();
            }
        }
    }
    if let Some(message) = v.message {
        let trimmed = message.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    format!("failed expression: {}", v.expression.trim())
}

/// Convenience real callers will need: does any real decision in this set
/// deny the request? (`is_error` decisions are already folded into
/// [`Action::Deny`]/[`Action::Admit`] by [`error_action`], so this needs
/// no separate error handling of its own.)
pub fn any_deny(decisions: &[Decision]) -> bool {
    decisions.iter().any(|d| d.action == Action::Deny)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_passing_validation_admits_with_no_message() {
        let object = json!({"replicas": 3});
        let v = [Validation { expression: "object.replicas > 0", message: None, reason: None, message_expression: None }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions, vec![Decision { action: Action::Admit, is_error: false, message: None, reason: None }]);
    }

    #[test]
    fn a_failing_validation_denies_with_its_own_declared_message_and_default_reason() {
        let object = json!({"replicas": 0});
        let v = [Validation { expression: "object.replicas > 0", message: Some("replicas must be positive"), reason: None, message_expression: None }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions[0].action, Action::Deny);
        assert_eq!(decisions[0].message.as_deref(), Some("replicas must be positive"));
        assert_eq!(decisions[0].reason.as_deref(), Some("Invalid"));
    }

    #[test]
    fn a_failing_validation_with_no_message_falls_back_to_the_expression_itself() {
        let object = json!({"replicas": 0});
        let v = [Validation { expression: "object.replicas > 0", message: None, reason: None, message_expression: None }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions[0].message.as_deref(), Some("failed expression: object.replicas > 0"));
    }

    #[test]
    fn a_declared_reason_overrides_the_default() {
        let object = json!({"replicas": 0});
        let v = [Validation { expression: "object.replicas > 0", message: None, reason: Some("Forbidden"), message_expression: None }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions[0].reason.as_deref(), Some("Forbidden"));
    }

    #[test]
    fn a_message_expression_wins_over_the_declared_message() {
        let object = json!({"replicas": 0, "name": "web"});
        let v = [Validation {
            expression: "object.replicas > 0",
            message: Some("static fallback"),
            reason: None,
            message_expression: Some("'replicas must be positive for ' + object.name"),
        }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions[0].message.as_deref(), Some("replicas must be positive for web"));
    }

    #[test]
    fn a_message_expression_that_fails_to_evaluate_falls_back_to_the_declared_message() {
        let object = json!({"replicas": 0});
        let v = [Validation { expression: "object.replicas > 0", message: Some("static fallback"), reason: None, message_expression: Some("object.nonexistent.field") }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions[0].message.as_deref(), Some("static fallback"));
    }

    #[test]
    fn a_message_expression_yielding_a_non_string_falls_back_to_the_declared_message() {
        let object = json!({"replicas": 0});
        let v = [Validation { expression: "object.replicas > 0", message: Some("static fallback"), reason: None, message_expression: Some("object.replicas") }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions[0].message.as_deref(), Some("static fallback"));
    }

    #[test]
    fn a_message_expression_containing_a_newline_falls_back_to_the_declared_message() {
        let object = json!({"replicas": 0});
        let v = [Validation { expression: "object.replicas > 0", message: Some("static fallback"), reason: None, message_expression: Some("'line one\\nline two'") }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions[0].message.as_deref(), Some("static fallback"));
    }

    #[test]
    fn a_message_expression_yielding_only_whitespace_falls_back_to_the_declared_message() {
        let object = json!({"replicas": 0});
        let v = [Validation { expression: "object.replicas > 0", message: Some("static fallback"), reason: None, message_expression: Some("'   '") }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions[0].message.as_deref(), Some("static fallback"));
    }

    #[test]
    fn a_message_expression_over_the_real_max_length_falls_back_to_the_declared_message() {
        let object = json!({"replicas": 0});
        let too_long = format!("'{}'", "x".repeat(MAX_MESSAGE_EXPRESSION_BYTES + 1));
        let v = [Validation { expression: "object.replicas > 0", message: Some("static fallback"), reason: None, message_expression: Some(&too_long) }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions[0].message.as_deref(), Some("static fallback"));
    }

    #[test]
    fn a_compile_error_with_fail_policy_fail_denies_and_is_marked_a_real_error() {
        let object = json!({});
        let v = [Validation { expression: "this is not valid cel (((", message: None, reason: None, message_expression: None }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions[0].action, Action::Deny);
        assert!(decisions[0].is_error);
    }

    #[test]
    fn a_compile_error_with_fail_policy_ignore_admits_but_is_still_marked_a_real_error() {
        let object = json!({});
        let v = [Validation { expression: "this is not valid cel (((", message: None, reason: None, message_expression: None }];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Ignore);
        assert_eq!(decisions[0].action, Action::Admit);
        assert!(decisions[0].is_error);
    }

    #[test]
    fn every_validation_is_evaluated_independently_no_short_circuit_on_the_first_failure() {
        let object = json!({"replicas": 0, "name": ""});
        let v = [
            Validation { expression: "object.replicas > 0", message: Some("replicas"), reason: None, message_expression: None },
            Validation { expression: "object.name != ''", message: Some("name"), reason: None, message_expression: None },
        ];
        let decisions = evaluate_validations(&v, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].action, Action::Deny);
        assert_eq!(decisions[1].action, Action::Deny);
        assert_eq!(decisions[0].message.as_deref(), Some("replicas"));
        assert_eq!(decisions[1].message.as_deref(), Some("name"));
    }

    #[test]
    fn any_deny_is_true_only_when_at_least_one_decision_denies() {
        assert!(!any_deny(&[Decision { action: Action::Admit, is_error: false, message: None, reason: None }]));
        assert!(any_deny(&[
            Decision { action: Action::Admit, is_error: false, message: None, reason: None },
            Decision { action: Action::Deny, is_error: false, message: Some("x".into()), reason: Some("Invalid".into()) },
        ]));
        assert!(!any_deny(&[]));
    }
}
