//! The single real per-policy decision `ValidatingAdmissionPolicy`
//! enforcement needs — composes the three standalone primitives this arc
//! has landed so far (`policy_matching`, `match_conditions`,
//! `policy_validations`) in real upstream's own real order, matching
//! `validator.Validate`'s own real shape (fetched and read directly,
//! cited in `policy_validations`'s own doc comment): first decide whether
//! this policy even applies to the request at all (`spec.matchConstraints`,
//! then `spec.matchConditions`), and only then run `spec.validations`. The
//! `evaluate_with_validation_vars` entry point keeps the upstream distinction
//! that `namespaceObject` is available to validations but not match conditions.
//!
//! This module owns no I/O and no CRUD — see this module's own
//! [`PolicyDefinition`] doc comment for what a real caller still has to
//! assemble before calling [`evaluate`]: a decoded `ValidatingAdmissionPolicy`
//! object's own fields, the request's namespace/object labels, and an
//! already-bound `object`/`oldObject`/`request`/`params` CEL variable set
//! (`policy_matching::build_request_object` is the `request` half of
//! that; `object`/`oldObject`/`params` construction from a real request
//! body is assembled by the storage-backed policy adapter before this pure
//! evaluator is called).

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
    /// A composed `spec.variables` expression failed after the policy's
    /// resource and match-condition gates passed.
    VariableError { error: String },
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

    /// The real "should a caller actually reject this request" question,
    /// folding [`PolicyOutcome::MatchConditionsError`] back in — unlike
    /// [`PolicyOutcome::denies`], since a real caller enforcing this
    /// policy needs both real outcomes treated as a denial. Safe to
    /// unconditionally include `MatchConditionsError` here (unlike
    /// `denies`, which deliberately excludes it): [`evaluate`] only ever
    /// produces that variant when `failurePolicy` is already `Fail` (an
    /// `Ignore` policy's own matching error becomes `NotApplicable`
    /// instead, matching real upstream's own real `matchConditions`
    /// semantics — see [`MatchResult::Ignored`]'s own real "skip this
    /// policy" meaning).
    ///
    /// Real upstream's own remaining condition — a real caller must still
    /// gate this on the binding's own `validationActions` containing
    /// `"Deny"` (`"Warn"`/`"Audit"` alone must not reject the request) —
    /// is deliberately **not** folded in here: `PolicyOutcome` only knows
    /// about one policy's own decision, not which binding produced it or
    /// what that binding's own `validationActions` said. [`validation_actions_deny`]
    /// is the separate, real primitive for that half.
    pub fn is_denial(&self) -> bool {
        self.denies() || matches!(self, PolicyOutcome::MatchConditionsError { .. } | PolicyOutcome::VariableError { .. })
    }

    /// The real message a caller should report for a denial — the first
    /// real `Deny` decision's own message for `Decided`, or every
    /// `matchConditions` error joined together for `MatchConditionsError`.
    /// `None` for `NotApplicable` and for a `Decided` outcome that doesn't
    /// actually deny (nothing to report).
    pub fn denial_message(&self) -> Option<String> {
        match self {
            PolicyOutcome::NotApplicable => None,
            PolicyOutcome::MatchConditionsError { errors } => Some(errors.join("; ")),
            PolicyOutcome::VariableError { error } => Some(error.clone()),
            PolicyOutcome::Decided(decisions) => decisions.iter().find(|d| d.action == policy_validations::Action::Deny).and_then(|d| d.message.clone()),
        }
    }
}

/// Real upstream's own `ValidatingAdmissionPolicyBinding.spec.
/// validationActions` gate: a validation/`matchConditions` failure only
/// actually rejects the request if the binding's own declared actions include
/// `"Deny"` — `"Warn"`/`"Audit"` alone report the failure without blocking it.
pub fn validation_actions_deny(actions: &[&str]) -> bool {
    actions.iter().any(|a| *a == "Deny")
}

pub fn validation_actions_warn(actions: &[&str]) -> bool {
    actions.iter().any(|a| *a == "Warn")
}

pub fn validation_actions_audit(actions: &[&str]) -> bool {
    actions.iter().any(|a| *a == "Audit")
}

pub fn validation_actions_report(actions: &[&str]) -> bool {
    validation_actions_deny(actions) || validation_actions_warn(actions) || validation_actions_audit(actions)
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
    evaluate_with_validation_vars(
        policy,
        operation,
        group,
        version,
        resource,
        subresource,
        namespace_labels,
        object_labels,
        vars,
        vars,
    )
}

/// Evaluate a policy with the real distinction between variables available
/// to `matchConditions` and variables available to `validations`.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_with_validation_vars(
    policy: &PolicyDefinition,
    operation: &str,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace_labels: &BTreeMap<String, String>,
    object_labels: &BTreeMap<String, String>,
    match_vars: &[(&'static str, &Value)],
    validation_vars: &[(&'static str, &Value)],
) -> PolicyOutcome {
    if let Err(outcome) = match_policy(policy, operation, group, version, resource, subresource, namespace_labels, object_labels, match_vars) {
        return outcome;
    }
    PolicyOutcome::Decided(policy_validations::evaluate_validations(policy.validations, validation_vars, policy.failure_policy))
}

/// Evaluate a policy after composing its declared variables. Variables are
/// deliberately composed only after resource and match-condition filtering,
/// matching the API contract that `matchConditions` cannot reference them.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_with_composed_variables(
    policy: &PolicyDefinition,
    operation: &str,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace_labels: &BTreeMap<String, String>,
    object_labels: &BTreeMap<String, String>,
    match_vars: &[(&'static str, &Value)],
    validation_vars: &[(&'static str, &Value)],
    variables: &[policy_matching::Variable<'_>],
) -> PolicyOutcome {
    if let Err(outcome) = match_policy(policy, operation, group, version, resource, subresource, namespace_labels, object_labels, match_vars) {
        return outcome;
    }
    let composed = match policy_matching::compose_variables(variables, validation_vars) {
        Ok(value) => value,
        Err(_error) if policy.failure_policy == FailurePolicy::Ignore => return PolicyOutcome::NotApplicable,
        Err(error) => return PolicyOutcome::VariableError { error },
    };
    let mut validation_vars = validation_vars.to_vec();
    validation_vars.push(("variables", &composed));
    PolicyOutcome::Decided(policy_validations::evaluate_validations(policy.validations, &validation_vars, policy.failure_policy))
}

fn match_policy(
    policy: &PolicyDefinition,
    operation: &str,
    group: &str,
    version: &str,
    resource: &str,
    subresource: &str,
    namespace_labels: &BTreeMap<String, String>,
    object_labels: &BTreeMap<String, String>,
    match_vars: &[(&'static str, &Value)],
) -> Result<(), PolicyOutcome> {
    if !policy_matching::matches_resource_rules(policy.resource_rules, policy.exclude_resource_rules, operation, group, version, resource, subresource) {
        return Err(PolicyOutcome::NotApplicable);
    }
    if !policy_matching::matches_label_selector(policy.namespace_selector, namespace_labels) {
        return Err(PolicyOutcome::NotApplicable);
    }
    if !policy_matching::matches_label_selector(policy.object_selector, object_labels) {
        return Err(PolicyOutcome::NotApplicable);
    }
    if !policy.match_conditions.is_empty() {
        match match_conditions::match_conditions(policy.match_conditions, match_vars, policy.failure_policy) {
            MatchResult::Matches => {}
            // A real `false` result and real upstream's own `Ignore`-policy
            // "skip this policy" outcome are both "this policy has nothing
            // to say about this request" from the caller's point of view —
            // matches `MatchResult::matches()`'s own real collapsing.
            MatchResult::DoesNotMatch { .. } | MatchResult::Ignored { .. } => return Err(PolicyOutcome::NotApplicable),
            MatchResult::Error { errors } => return Err(PolicyOutcome::MatchConditionsError { errors }),
        }
    }
    Ok(())
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

    #[test]
    fn is_denial_folds_a_match_conditions_error_back_in_unlike_denies() {
        let outcome = PolicyOutcome::MatchConditionsError { errors: vec!["boom".to_string()] };
        assert!(!outcome.denies(), "denies() deliberately excludes MatchConditionsError");
        assert!(outcome.is_denial(), "is_denial() deliberately folds it back in");
    }

    #[test]
    fn is_denial_is_false_for_not_applicable_and_a_clean_admit() {
        assert!(!PolicyOutcome::NotApplicable.is_denial());
        let admit = PolicyOutcome::Decided(vec![Decision { action: policy_validations::Action::Admit, is_error: false, message: None, reason: None }]);
        assert!(!admit.is_denial());
    }

    #[test]
    fn namespace_object_is_available_to_validations_but_not_match_conditions() {
        let rules = [ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*"] }];
        let validations = [Validation { expression: "namespaceObject.metadata.name == 'default'", message: None, reason: None, message_expression: None }];
        let conditions = [MatchCondition { name: "request-is-create", expression: "request.operation == 'CREATE'" }];
        let policy = PolicyDefinition {
            resource_rules: &rules,
            exclude_resource_rules: &[],
            namespace_selector: None,
            object_selector: None,
            match_conditions: &conditions,
            validations: &validations,
            failure_policy: FailurePolicy::Fail,
        };
        let request = json!({"operation": "CREATE"});
        let namespace = json!({"metadata": {"name": "default"}});
        let match_vars = &[("request", &request)];
        let validation_vars = &[("request", &request), ("namespaceObject", &namespace)];
        let outcome = evaluate_with_validation_vars(&policy, "CREATE", "", "v1", "pods", "", &labels(&[]), &labels(&[]), match_vars, validation_vars);
        assert_eq!(outcome, PolicyOutcome::Decided(vec![Decision { action: Action::Admit, is_error: false, message: None, reason: None }]));

        let conditions = [MatchCondition { name: "namespace-is-default", expression: "namespaceObject.metadata.name == 'default'" }];
        let policy = PolicyDefinition { match_conditions: &conditions, ..policy };
        assert!(matches!(evaluate_with_validation_vars(&policy, "CREATE", "", "v1", "pods", "", &labels(&[]), &labels(&[]), match_vars, validation_vars), PolicyOutcome::MatchConditionsError { .. }));
    }

    #[test]
    fn composed_variables_feed_validations_after_match_conditions() {
        let rules = [ResourceRule { operations: &["CREATE"], api_groups: &[""], api_versions: &["v1"], resources: &["pods"] }];
        let validations = [Validation { expression: "variables.minimum == 5", message: Some("minimum must be five"), reason: None, message_expression: None }];
        let policy = base_policy(&rules, &validations);
        let object = json!({"spec": {"replicas": 3}});
        let request = json!({"operation": "CREATE"});
        let match_vars = policy_matching::build_eval_vars(Some(&object), None, &request, None);
        let validation_vars = policy_matching::build_eval_vars_with_namespace(Some(&object), None, &request, None, None);
        let variables = [
            policy_matching::Variable { name: "replicas", expression: "object.spec.replicas" },
            policy_matching::Variable { name: "minimum", expression: "variables.replicas + 2" },
        ];
        let outcome = evaluate_with_composed_variables(&policy, "CREATE", "", "v1", "pods", "", &labels(&[]), &labels(&[]), &match_vars, &validation_vars, &variables);
        assert_eq!(outcome, PolicyOutcome::Decided(vec![Decision { action: Action::Admit, is_error: false, message: None, reason: None }]));
    }

    #[test]
    fn variable_composition_is_skipped_when_a_match_condition_excludes_the_request() {
        let rules = [ResourceRule { operations: &["CREATE"], api_groups: &[""], api_versions: &["v1"], resources: &["pods"] }];
        let conditions = [MatchCondition { name: "never", expression: "false" }];
        let policy = PolicyDefinition { match_conditions: &conditions, ..base_policy(&rules, &[]) };
        let request = json!({"operation": "CREATE"});
        let match_vars = policy_matching::build_eval_vars(None, None, &request, None);
        let validation_vars = policy_matching::build_eval_vars_with_namespace(None, None, &request, None, None);
        let variables = [policy_matching::Variable { name: "broken", expression: "this is not valid cel (((" }];
        assert_eq!(evaluate_with_composed_variables(&policy, "CREATE", "", "v1", "pods", "", &labels(&[]), &labels(&[]), &match_vars, &validation_vars, &variables), PolicyOutcome::NotApplicable);
    }

    #[test]
    fn denial_message_reports_the_first_real_deny_for_a_decided_outcome() {
        let outcome = PolicyOutcome::Decided(vec![
            Decision { action: policy_validations::Action::Admit, is_error: false, message: None, reason: None },
            Decision { action: policy_validations::Action::Deny, is_error: false, message: Some("replicas must be positive".to_string()), reason: Some("Invalid".to_string()) },
        ]);
        assert_eq!(outcome.denial_message().as_deref(), Some("replicas must be positive"));
    }

    #[test]
    fn denial_message_joins_every_match_conditions_error() {
        let outcome = PolicyOutcome::MatchConditionsError { errors: vec!["broken: parse error".to_string(), "also-broken: runtime error".to_string()] };
        assert_eq!(outcome.denial_message().as_deref(), Some("broken: parse error; also-broken: runtime error"));
    }

    #[test]
    fn denial_message_is_none_for_not_applicable_and_a_clean_admit() {
        assert_eq!(PolicyOutcome::NotApplicable.denial_message(), None);
        let admit = PolicyOutcome::Decided(vec![Decision { action: policy_validations::Action::Admit, is_error: false, message: None, reason: None }]);
        assert_eq!(admit.denial_message(), None);
    }

    #[test]
    fn validation_actions_deny_requires_the_real_deny_action_by_name() {
        assert!(validation_actions_deny(&["Deny"]));
        assert!(validation_actions_deny(&["Audit", "Deny"]));
        assert!(!validation_actions_deny(&["Warn"]));
        assert!(!validation_actions_deny(&["Audit"]));
        assert!(!validation_actions_deny(&[]));
    }

    #[test]
    fn validation_actions_report_preserves_warn_and_audit_only_bindings() {
        assert!(validation_actions_warn(&["Warn"]));
        assert!(validation_actions_audit(&["Audit"]));
        assert!(validation_actions_report(&["Warn"]));
        assert!(validation_actions_report(&["Audit"]));
        assert!(!validation_actions_report(&[]));
    }
}
