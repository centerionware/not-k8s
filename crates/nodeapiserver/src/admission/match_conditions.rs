//! Real upstream's own `matchconditions.Matcher`
//! (`k8s.io/apiserver/pkg/admission/plugin/webhook/matchconditions/
//! matcher.go`, fetched and read directly) — the real CEL-based
//! pre-filter shared by `ValidatingWebhookConfiguration`/
//! `MutatingWebhookConfiguration`'s own `spec.matchConditions` and
//! `ValidatingAdmissionPolicy`'s own `spec.matchConditions`: a list of
//! named CEL expressions that must **all** evaluate `true` for the
//! webhook/policy to apply to this request at all. The first one to
//! evaluate `false` short-circuits the rest (real upstream's own
//! comment: "skip calling webhook always" — matches its own real
//! ordering, not a simplification: an earlier condition's own real
//! evaluation *error* is recorded but doesn't stop the scan, while a
//! real `false` result does, immediately, even past conditions that
//! already errored).
//!
//! **Not yet wired to anything real**: this crate has no
//! `ValidatingWebhookConfiguration`/`MutatingWebhookConfiguration` or
//! `ValidatingAdmissionPolicy` support at all yet (both genuinely
//! not started — `docs/APISERVER.md`'s own Group J and `cel_ext`
//! sections name this) — landed as the real, standalone, pure primitive
//! either of those would need, the same "land the primitive first"
//! discipline this whole arc has used throughout. Real upstream's own
//! `object`/`oldObject`/`request`/`params`/`authorizer` CEL variable
//! bindings aren't built here either — this module takes an already-
//! bound `vars` slice (`cel_ext::eval_bool_with_vars`'s own shape),
//! leaving "how those real variables get constructed from an actual
//! admission request" to whichever future module actually wires a
//! webhook or policy engine in.

use crate::cel_ext::eval_bool_with_vars_and_deadline;
use serde_json::Value;
use std::time::Duration;

/// Real upstream's own `RuntimeCELCostBudgetMatchConditions`'s wall-
/// clock stand-in — `cel_ext`'s own module doc covers why a deadline
/// substitutes for real cost accounting throughout this crate.
/// `~0.1s`, the same real `PerCallLimit`-derived bound
/// `apiextensions::cel_evaluate`'s own per-rule deadline already uses,
/// since match conditions are evaluated per-condition too, not against
/// one shared aggregate budget any more than that module's own rules
/// are (both real, separate, not-yet-closed gaps).
const MATCH_CONDITION_DEADLINE: Duration = Duration::from_millis(100);

/// Real upstream's own `v1.FailurePolicyType` — what happens when a
/// match condition itself fails to *evaluate* (a compile error, a
/// runtime error), distinct from evaluating to a real `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePolicy {
    /// Real upstream's own default.
    Fail,
    Ignore,
}

/// One real `v1.MatchCondition` — `name` is what a real `false` result
/// or evaluation error gets reported against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchCondition<'a> {
    pub name: &'a str,
    pub expression: &'a str,
}

/// Real upstream's own `MatchResult`, refined into a real Rust enum —
/// upstream's own struct flattens the `Ignore`-driven skip and a real
/// `false` result into the same `{Matches: false}` shape; kept as two
/// distinct variants here since they're genuinely different real
/// reasons (`Ignored` still names which conditions actually errored,
/// which upstream's own flatter shape discards) — [`MatchResult::
/// matches`] collapses both back to a plain `bool` for a caller that
/// only cares about the real accept/reject decision, the same
/// information real upstream's own `Matches` field alone carries.
#[derive(Debug, Clone, PartialEq)]
pub enum MatchResult {
    /// Every match condition evaluated `true` (or there were none) —
    /// the webhook/policy applies to this request.
    Matches,
    /// At least one match condition evaluated `false` — real upstream's
    /// own `FailedConditionName`.
    DoesNotMatch { failed_condition_name: String },
    /// One or more conditions failed to evaluate and `failure_policy`
    /// is [`FailurePolicy::Ignore`] — real upstream's own "skip call to
    /// webhook" outcome, distinct from a real `false` result (see this
    /// type's own doc comment).
    Ignored { errors: Vec<String> },
    /// One or more conditions failed to evaluate and `failure_policy`
    /// is [`FailurePolicy::Fail`] — real upstream's own aggregate
    /// error, which the caller must surface as a genuine admission
    /// failure (not silently treated as "doesn't match").
    Error { errors: Vec<String> },
}

impl MatchResult {
    /// Collapses to the plain accept/reject decision real upstream's
    /// own `Matches` bool field alone carries — `false` for
    /// `DoesNotMatch`/`Ignored` alike, matching real upstream's own
    /// flattened struct exactly; [`MatchResult::Error`] has no boolean
    /// equivalent at all (it isn't a match decision, it's a failure the
    /// caller must handle separately), so this returns `None` for it
    /// rather than silently picking either `true` or `false`.
    pub fn matches(&self) -> Option<bool> {
        match self {
            MatchResult::Matches => Some(true),
            MatchResult::DoesNotMatch { .. } | MatchResult::Ignored { .. } => Some(false),
            MatchResult::Error { .. } => None,
        }
    }
}

/// Real upstream's own `matcher.Match` — see this module's own doc
/// comment for the real, named "not yet wired" scope and the real
/// `vars` shape it expects.
pub fn match_conditions(conditions: &[MatchCondition], vars: &[(&'static str, &Value)], failure_policy: FailurePolicy) -> MatchResult {
    let mut errors: Vec<String> = Vec::new();
    for condition in conditions {
        match eval_bool_with_vars_and_deadline(condition.expression, vars, MATCH_CONDITION_DEADLINE) {
            Ok(true) => {}
            // Real upstream's own real ordering: a `false` result wins
            // immediately, even over conditions that already errored
            // earlier in the same scan.
            Ok(false) => return MatchResult::DoesNotMatch { failed_condition_name: condition.name.to_string() },
            Err(e) => errors.push(format!("{}: {e}", condition.name)),
        }
    }
    if errors.is_empty() {
        return MatchResult::Matches;
    }
    match failure_policy {
        FailurePolicy::Fail => MatchResult::Error { errors },
        FailurePolicy::Ignore => MatchResult::Ignored { errors },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_conditions_at_all_always_matches() {
        let result = match_conditions(&[], &[], FailurePolicy::Fail);
        assert_eq!(result, MatchResult::Matches);
    }

    #[test]
    fn every_condition_true_matches() {
        let object = json!({"replicas": 3});
        let conditions = [MatchCondition { name: "has-replicas", expression: "object.replicas > 0" }];
        let result = match_conditions(&conditions, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(result, MatchResult::Matches);
    }

    #[test]
    fn a_false_condition_short_circuits_with_its_own_real_name() {
        let object = json!({"replicas": 0});
        let conditions = [
            MatchCondition { name: "has-replicas", expression: "object.replicas > 0" },
            MatchCondition { name: "unreached", expression: "true" },
        ];
        let result = match_conditions(&conditions, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(result, MatchResult::DoesNotMatch { failed_condition_name: "has-replicas".to_string() });
    }

    #[test]
    fn a_false_condition_wins_even_over_an_earlier_error_in_the_same_scan() {
        let object = json!({"replicas": 0});
        let conditions = [
            MatchCondition { name: "broken", expression: "object.nonexistent.field" },
            MatchCondition { name: "false-one", expression: "object.replicas > 0" },
        ];
        let result = match_conditions(&conditions, &[("object", &object)], FailurePolicy::Fail);
        assert_eq!(result, MatchResult::DoesNotMatch { failed_condition_name: "false-one".to_string() }, "a real false result must win over a prior evaluation error");
    }

    #[test]
    fn an_evaluation_error_with_fail_policy_fail_is_a_real_error() {
        let object = json!({});
        let conditions = [MatchCondition { name: "broken", expression: "this is not valid cel(((" }];
        let result = match_conditions(&conditions, &[("object", &object)], FailurePolicy::Fail);
        assert!(matches!(result, MatchResult::Error { .. }));
        assert_eq!(result.matches(), None);
    }

    #[test]
    fn an_evaluation_error_with_fail_policy_ignore_is_treated_as_no_match() {
        let object = json!({});
        let conditions = [MatchCondition { name: "broken", expression: "this is not valid cel(((" }];
        let result = match_conditions(&conditions, &[("object", &object)], FailurePolicy::Ignore);
        assert!(matches!(result, MatchResult::Ignored { .. }));
        assert_eq!(result.matches(), Some(false));
    }

    #[test]
    fn matches_helper_collapses_every_real_outcome_to_the_right_bool() {
        assert_eq!(MatchResult::Matches.matches(), Some(true));
        assert_eq!(MatchResult::DoesNotMatch { failed_condition_name: "x".to_string() }.matches(), Some(false));
        assert_eq!(MatchResult::Ignored { errors: vec![] }.matches(), Some(false));
        assert_eq!(MatchResult::Error { errors: vec![] }.matches(), None);
    }
}
