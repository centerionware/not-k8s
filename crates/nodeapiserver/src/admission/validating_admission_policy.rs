//! The single real per-policy decision `ValidatingAdmissionPolicy`
//! enforcement needs — composes the three standalone primitives this arc
//! has landed so far (`policy_matching`, `match_conditions`,
//! `policy_validations`) in real upstream's own real order, matching
//! `validator.Validate`'s own real shape (fetched and read directly,
//! cited in `policy_validations`'s own doc comment): first decide whether
//! this policy even applies to the request at all (`spec.matchConstraints`,
//! then `spec.matchConditions`), and only then run `spec.validations`.
//!
//! This module owns no I/O and no CRUD — see this module's own
//! [`PolicyDefinition`] doc comment for what a real caller still has to
//! assemble before calling [`evaluate`]: a decoded `ValidatingAdmissionPolicy`
//! object's own fields, the request's namespace/object labels, and an
//! already-bound `object`/`oldObject`/`request`/`params` CEL variable set
//! (`policy_matching::build_request_object` is the `request` half of
//! that; `object`/`oldObject`/`params` construction from a real request
//! body is still real, separate, not-yet-started work, named in
//! `admission`'s own module doc comment).

use super::match_conditions::{self, FailurePolicy, MatchCondition, MatchResult};
use super::policy_matching::{self, ResourceRule};
use super::policy_validations::{self, Decision, Validation};
use serde_json::Value;
use std::collections::BTreeMap;

/// Everything [`evaluate`] needs from one real `ValidatingAdmissionPolicy`
/// object's own `spec` — a plain, borrowed view a caller builds from the
/// decoded object, not a copy of the wire type itself (this crate's own
/// established pattern: [`super::match_conditions::MatchCondition`] and
/// [`super::policy_validations::Validation`] are the same shape of
/// borrowed view, not owned structs).
///
/// Two lifetimes for the same reason [`ResourceRule`] itself has two —
/// `'a` is how long this struct's own slices live, `'b` is how long the
/// `&str`/`Value` data underneath them lives. `admission::policy_decode`'s
/// `DecodedPolicy` is the real caller this shape exists for: its own
/// `resource_rules()`/`exclude_resource_rules()` hand back a freshly
/// built `Vec<ResourceRule<'_, 'b>>` borrowing `DecodedPolicy`'s own
/// backing storage (`'a`, tied to that one call), while every `&str`
/// inside still points directly at the original decoded
/// `serde_json::Value` (`'b`).
#[derive(Debug, Clone, Copy)]
pub struct PolicyDefinition<'a, 'b> {
    pub resource_rules: &'a [ResourceRule<'a, 'b>],
    pub exclude_resource_rules: &'a [ResourceRule<'a, 'b>],
    pub namespace_selector: Option<&'b Value>,
    pub object_selector: Option<&'b Value>,
    pub match_conditions: &'a [MatchCondition<'b>],
    pub validations: &'a [Validation<'b>],
    pub failure_policy: FailurePolicy,
}

/// The real per-policy outcome — real upstream's own `ValidateResult`,
/// collapsed to what this crate's own primitives can produce.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyOutcome {
    /// `spec.matchConstraints` (resource rules, `namespaceSelector`,
    /// `objectSelector`) didn't match, or `spec.matchConditions` real
    /// `false` result — or, real upstream's own `Ignore`-policy "skip
    /// this policy" outcome for a `matchConditions` evaluation error —
    /// excluded it. Either way, this policy has nothing to say about this
    /// request, matching real upstream's own empty `ValidateResult{}`.
    NotApplicable,
    /// `spec.matchConditions` failed to *evaluate* and `failurePolicy` is
    /// `Fail` — a real admission error, distinct from a real `false`
    /// match result. Real upstream's own `Evaluation: EvalError` at the
    /// `matchConditions` stage.
    MatchConditionsError { errors: Vec<String> },
    /// The policy applied and its `spec.validations` were evaluated —
    /// one [`Decision`] per rule, same shape
    /// [`policy_validations::evaluate_validations`] already returns.
    Decided(Vec<Decision>),
}

impl PolicyOutcome {
    /// `true` only for [`PolicyOutcome::Decided`] carrying at least one
    /// real `Deny` — [`PolicyOutcome::MatchConditionsError`] is a real
    /// failure a caller must handle on its own terms (it isn't a
    /// validation denial), so this deliberately doesn't fold it in.
    pub fn denies(&self) -> bool {
        matches!(self, PolicyOutcome::Decided(decisions) if policy_validations::any_deny(decisions))
    }
}

/// Real upstream's own real order: `matchConstraints` (resource rules,
/// then `namespaceSelector`, then `objectSelector`) → `matchConditions`
/// → `validations`. Each stage can only narrow further; nothing after a
/// `NotApplicable`/`MatchConditionsError` verdict runs.
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    policy: &PolicyDefinition,
    operation: &str,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace_labels: &BTreeMap<String, String>,
    object_labels: &BTreeMap<String, String>,
    vars: &[(&'static str, &Value)],
) -> PolicyOutcome {
    if !policy_matching::matches_resource_rules(policy.resource_rules, policy.exclude_resource_rules, operation, group, version, resource, subresource) {
        return PolicyOutcome::NotApplicable;
    }
    if !policy_matching::matches_label_selector(policy.namespace_selector, namespace_labels) {
        return PolicyOutcome::NotApplicable;
    }
    if !policy_matching::matches_label_selector(policy.object_selector, object_labels) {
        return PolicyOutcome::NotApplicable;
    }
    if !policy.match_conditions.is_empty() {
        match match_conditions::match_conditions(policy.match_conditions, vars, policy.failure_policy) {
            MatchResult::Matches => {}
            // A real `false` result and real upstream's own `Ignore`-policy
            // "skip this policy" outcome are both "this policy has nothing
            // to say about this request" from the caller's point of view —
            // matches `MatchResult::matches()`'s own real collapsing.
            MatchResult::DoesNotMatch { .. } | MatchResult::Ignored { .. } => return PolicyOutcome::NotApplicable,
            MatchResult::Error { errors } => return PolicyOutcome::MatchConditionsError { errors },
        }
    }
    PolicyOutcome::Decided(policy_validations::evaluate_validations(policy.validations, vars, policy.failure_policy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::policy_validations::Action;
    use serde_json::json;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn base_policy<'a>(resource_rules: &'a [ResourceRule<'a, 'a>], validations: &'a [Validation<'a>]) -> PolicyDefinition<'a, 'a> {
        PolicyDefinition { resource_rules, exclude_resource_rules: &[], namespace_selector: None, object_selector: None, match_conditions: &[], validations, failure_policy: FailurePolicy::Fail }
    }

    #[test]
    fn a_request_outside_the_resource_rules_is_not_applicable() {
        let rules = [ResourceRule { operations: &["CREATE"], api_groups: &["apps"], api_versions: &["v1"], resources: &["deployments"] }];
        let validations = [Validation { expression: "true", message: None, reason: None, message_expression: None }];
        let policy = base_policy(&rules, &validations);
        let object = json!({});
        let outcome = evaluate(&policy, "CREATE", "", "v1", "pods", "", &labels(&[]), &labels(&[]), &[("object", &object)]);
        assert_eq!(outcome, PolicyOutcome::NotApplicable);
    }

    #[test]
    fn a_namespace_selector_mismatch_is_not_applicable_even_when_resource_rules_match() {
        let rules = [ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*"] }];
        let validations = [Validation { expression: "false", message: None, reason: None, message_expression: None }];
        let mut policy = base_policy(&rules, &validations);
        let sel = json!({"matchLabels": {"env": "prod"}});
        policy.namespace_selector = Some(&sel);
        let object = json!({});
        let outcome = evaluate(&policy, "CREATE", "", "v1", "pods", "", &labels(&[("env", "dev")]), &labels(&[]), &[("object", &object)]);
        assert_eq!(outcome, PolicyOutcome::NotApplicable);
    }

    #[test]
    fn an_object_selector_mismatch_is_not_applicable() {
        let rules = [ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*"] }];
        let validations = [Validation { expression: "false", message: None, reason: None, message_expression: None }];
        let mut policy = base_policy(&rules, &validations);
        let sel = json!({"matchLabels": {"tier": "frontend"}});
        policy.object_selector = Some(&sel);
        let object = json!({});
        let outcome = evaluate(&policy, "CREATE", "", "v1", "pods", "", &labels(&[]), &labels(&[("tier", "backend")]), &[("object", &object)]);
        assert_eq!(outcome, PolicyOutcome::NotApplicable);
    }

    #[test]
    fn a_false_match_condition_makes_the_whole_policy_not_applicable_before_validations_ever_run() {
        let rules = [ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*"] }];
        // If this validation ran, it would deny -- proving matchConditions
        // really did short-circuit before it.
        let validations = [Validation { expression: "false", message: None, reason: None, message_expression: None }];
        let mut policy = base_policy(&rules, &validations);
        let conditions = [MatchCondition { name: "never", expression: "false" }];
        policy.match_conditions = &conditions;
        let object = json!({});
        let outcome = evaluate(&policy, "CREATE", "", "v1", "pods", "", &labels(&[]), &labels(&[]), &[("object", &object)]);
        assert_eq!(outcome, PolicyOutcome::NotApplicable);
    }

    #[test]
    fn a_match_condition_evaluation_error_with_fail_policy_is_surfaced_distinctly() {
        let rules = [ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*"] }];
        let validations: [Validation; 0] = [];
        let mut policy = base_policy(&rules, &validations);
        let conditions = [MatchCondition { name: "broken", expression: "this is not valid cel(((" }];
        policy.match_conditions = &conditions;
        let object = json!({});
        let outcome = evaluate(&policy, "CREATE", "", "v1", "pods", "", &labels(&[]), &labels(&[]), &[("object", &object)]);
        assert!(matches!(outcome, PolicyOutcome::MatchConditionsError { .. }));
        assert!(!outcome.denies(), "a real error is not the same real outcome as a validation denial");
    }

    #[test]
    fn matching_and_passing_every_validation_admits() {
        let rules = [ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*"] }];
        let validations = [Validation { expression: "object.replicas > 0", message: None, reason: None, message_expression: None }];
        let policy = base_policy(&rules, &validations);
        let object = json!({"replicas": 3});
        let outcome = evaluate(&policy, "CREATE", "", "v1", "pods", "", &labels(&[]), &labels(&[]), &[("object", &object)]);
        match &outcome {
            PolicyOutcome::Decided(decisions) => assert_eq!(decisions[0].action, Action::Admit),
            other => panic!("expected Decided, got {other:?}"),
        }
        assert!(!outcome.denies());
    }

    #[test]
    fn matching_and_failing_a_validation_denies() {
        let rules = [ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*"] }];
        let validations = [Validation { expression: "object.replicas > 0", message: Some("replicas must be positive"), reason: None, message_expression: None }];
        let policy = base_policy(&rules, &validations);
        let object = json!({"replicas": 0});
        let outcome = evaluate(&policy, "CREATE", "", "v1", "pods", "", &labels(&[]), &labels(&[]), &[("object", &object)]);
        assert!(outcome.denies());
        match outcome {
            PolicyOutcome::Decided(decisions) => assert_eq!(decisions[0].message.as_deref(), Some("replicas must be positive")),
            other => panic!("expected Decided, got {other:?}"),
        }
    }
}
