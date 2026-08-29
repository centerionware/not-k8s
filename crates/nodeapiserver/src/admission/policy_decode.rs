//! Decodes a real `ValidatingAdmissionPolicy` object's own `spec` — the
//! wire JSON shape `server::rest`'s generic verbs already store/read
//! unmodified (confirmed live by
//! `tests/validating_admission_policy_roundtrip.rs`), field names
//! verified directly against the vendored OpenAPI schema
//! (`vendor/openapi-spec/v3/apis__admissionregistration.k8s.io__v1_
//! openapi.json`'s own `ValidatingAdmissionPolicySpec`/`MatchResources`/
//! `NamedRuleWithOperations`/`MatchCondition`/`Validation` schemas, not
//! assumed from memory) — into the owned intermediate representation
//! [`validating_admission_policy::PolicyDefinition`]'s own borrowed view
//! needs. The storage-backed policy adapter uses this representation for
//! real admission requests as well as the unit-test fixtures.
//!
//! Kept deliberately dumb: no schema validation, no defaulting (real
//! upstream's own CRD-acceptance-time validation is what's supposed to
//! guarantee a policy object's own shape by the time it's persisted; this
//! module tolerates a missing/malformed field by treating it as
//! absent/empty rather than erroring, the same posture `cacher::selector`'s
//! own `object_labels` already takes for a malformed `metadata.labels`).
//!
//! **Two-step shape, not one `as_definition()` call**: [`DecodedPolicy`]
//! owns its own backing storage for `resourceRules`/`excludeResourceRules`
//! (each entry's own `operations`/`apiGroups`/`apiVersions`/`resources`
//! arrays), and [`ResourceRule`] itself borrows from *that* storage, not
//! from the original `serde_json::Value` directly — a real self-referential-
//! struct shape no single method returning
//! [`validating_admission_policy::PolicyDefinition`] by value could express
//! safely, since the returned struct would need to borrow from a local
//! `Vec` that method call's own stack frame just dropped. [`DecodedPolicy::
//! resource_rules`]/[`DecodedPolicy::exclude_resource_rules`] hand back a
//! freshly built `Vec<ResourceRule>` instead — a caller binds that to a
//! local, then builds the `PolicyDefinition` referencing both it and
//! `DecodedPolicy`'s own `match_conditions`/`validations`/selector fields
//! directly (`MatchCondition`/`Validation`/`Value` don't have this
//! problem: each is a single level of `&str`/`&Value` borrowing straight
//! from the original decoded object, no intermediate storage needed).

use super::match_conditions::{FailurePolicy, MatchCondition};
use super::policy_matching::{ResourceRule, Variable};
use super::policy_validations::Validation;
use serde_json::Value;

/// One decoded `NamedRuleWithOperations` entry — owns the four `Vec<&str>`
/// arrays [`ResourceRule`] itself only borrows.
struct DecodedResourceRule<'b> {
    operations: Vec<&'b str>,
    api_groups: Vec<&'b str>,
    api_versions: Vec<&'b str>,
    resources: Vec<&'b str>,
}

impl<'b> DecodedResourceRule<'b> {
    fn decode(rule: &'b Value) -> Self {
        DecodedResourceRule { operations: str_array(rule, "operations"), api_groups: str_array(rule, "apiGroups"), api_versions: str_array(rule, "apiVersions"), resources: str_array(rule, "resources") }
    }

    fn as_rule(&self) -> ResourceRule<'_, 'b> {
        ResourceRule { operations: &self.operations, api_groups: &self.api_groups, api_versions: &self.api_versions, resources: &self.resources }
    }
}

fn str_array<'b>(v: &'b Value, field: &str) -> Vec<&'b str> {
    v.get(field).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

fn decode_match_condition(v: &Value) -> Option<MatchCondition<'_>> {
    Some(MatchCondition { name: v.get("name").and_then(Value::as_str)?, expression: v.get("expression").and_then(Value::as_str)? })
}

fn decode_validation(v: &Value) -> Option<Validation<'_>> {
    Some(Validation {
        expression: v.get("expression").and_then(Value::as_str)?,
        message: v.get("message").and_then(Value::as_str),
        reason: v.get("reason").and_then(Value::as_str),
        message_expression: v.get("messageExpression").and_then(Value::as_str),
    })
}

fn decode_variable(v: &Value) -> Option<Variable<'_>> {
    Some(Variable {
        name: v.get("name").and_then(Value::as_str)?,
        expression: v.get("expression").and_then(Value::as_str)?,
    })
}

/// The owned decode of one real `ValidatingAdmissionPolicy` object's
/// `spec` — see this module's own doc comment for the real two-step shape
/// a caller uses this through.
pub struct DecodedPolicy<'b> {
    resource_rules: Vec<DecodedResourceRule<'b>>,
    exclude_resource_rules: Vec<DecodedResourceRule<'b>>,
    /// `spec.matchConstraints.namespaceSelector` — `None` for both a
    /// genuinely absent field and an explicit JSON `null`, matching real
    /// upstream's own "default to the empty `LabelSelector`, which
    /// matches everything" (`policy_matching::matches_label_selector`'s
    /// own `None` case already implements that "matches everything"
    /// behavior).
    pub namespace_selector: Option<&'b Value>,
    pub object_selector: Option<&'b Value>,
    pub match_conditions: Vec<MatchCondition<'b>>,
    pub validations: Vec<Validation<'b>>,
    pub variables: Vec<Variable<'b>>,
    /// `spec.failurePolicy` — real upstream's own default (`Fail`) when
    /// absent or set to anything other than the real `"Ignore"` string.
    pub failure_policy: FailurePolicy,
}

impl<'b> DecodedPolicy<'b> {
    /// Decodes `policy`'s own `spec` — `policy` is expected to be one
    /// `ValidatingAdmissionPolicy` object (e.g. one `items[]` entry from a
    /// real `LIST`), not the whole list wrapper.
    pub fn decode(policy: &'b Value) -> Self {
        let spec = policy.get("spec");
        let constraints = spec.and_then(|s| s.get("matchConstraints"));
        let resource_rules = constraints.and_then(|c| c.get("resourceRules")).and_then(Value::as_array).map(|a| a.iter().map(DecodedResourceRule::decode).collect()).unwrap_or_default();
        let exclude_resource_rules = constraints.and_then(|c| c.get("excludeResourceRules")).and_then(Value::as_array).map(|a| a.iter().map(DecodedResourceRule::decode).collect()).unwrap_or_default();
        let namespace_selector = constraints.and_then(|c| c.get("namespaceSelector")).filter(|v| !v.is_null());
        let object_selector = constraints.and_then(|c| c.get("objectSelector")).filter(|v| !v.is_null());
        let match_conditions = spec.and_then(|s| s.get("matchConditions")).and_then(Value::as_array).map(|a| a.iter().filter_map(decode_match_condition).collect()).unwrap_or_default();
        let validations = spec.and_then(|s| s.get("validations")).and_then(Value::as_array).map(|a| a.iter().filter_map(decode_validation).collect()).unwrap_or_default();
        let variables = spec.and_then(|s| s.get("variables")).and_then(Value::as_array).map(|a| a.iter().filter_map(decode_variable).collect()).unwrap_or_default();
        let failure_policy = match spec.and_then(|s| s.get("failurePolicy")).and_then(Value::as_str) {
            Some("Ignore") => FailurePolicy::Ignore,
            _ => FailurePolicy::Fail,
        };
        DecodedPolicy { resource_rules, exclude_resource_rules, namespace_selector, object_selector, match_conditions, validations, variables, failure_policy }
    }

    /// `spec.matchConstraints.resourceRules`, freshly built each call —
    /// see this module's own doc comment for why this can't be a field
    /// caller can borrow directly.
    pub fn resource_rules(&self) -> Vec<ResourceRule<'_, 'b>> {
        self.resource_rules.iter().map(DecodedResourceRule::as_rule).collect()
    }

    /// `spec.matchConstraints.excludeResourceRules` — see
    /// [`DecodedPolicy::resource_rules`].
    pub fn exclude_resource_rules(&self) -> Vec<ResourceRule<'_, 'b>> {
        self.exclude_resource_rules.iter().map(DecodedResourceRule::as_rule).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::policy_matching;
    use crate::admission::validating_admission_policy::{self, PolicyDefinition};
    use serde_json::json;
    use std::collections::BTreeMap;

    /// The real end-to-end shape a caller will actually use: decode a
    /// real `ValidatingAdmissionPolicy` object, build a
    /// [`PolicyDefinition`] from it, and evaluate — proving the two-step
    /// borrow shape this module's own doc comment describes actually
    /// compiles and behaves correctly together, not just each half in
    /// isolation.
    #[test]
    fn a_real_policy_document_decodes_and_evaluates_end_to_end() {
        let policy = json!({
            "apiVersion": "admissionregistration.k8s.io/v1",
            "kind": "ValidatingAdmissionPolicy",
            "metadata": {"name": "replicas-must-be-positive"},
            "spec": {
                "failurePolicy": "Fail",
                "matchConstraints": {
                    "resourceRules": [{"apiGroups": ["apps"], "apiVersions": ["v1"], "operations": ["CREATE"], "resources": ["deployments"]}],
                },
                "matchConditions": [{"name": "not-kube-system", "expression": "request.namespace != 'kube-system'"}],
                "validations": [{"expression": "object.spec.replicas > 0", "message": "replicas must be positive"}],
            },
        });
        let decoded = DecodedPolicy::decode(&policy);
        assert_eq!(decoded.failure_policy, FailurePolicy::Fail);
        assert_eq!(decoded.match_conditions.len(), 1);
        assert_eq!(decoded.validations.len(), 1);

        let resource_rules = decoded.resource_rules();
        let exclude_resource_rules = decoded.exclude_resource_rules();
        let def = PolicyDefinition {
            resource_rules: &resource_rules,
            exclude_resource_rules: &exclude_resource_rules,
            namespace_selector: decoded.namespace_selector,
            object_selector: decoded.object_selector,
            match_conditions: &decoded.match_conditions,
            validations: &decoded.validations,
            failure_policy: decoded.failure_policy,
        };

        let request = json!({"namespace": "default"});
        let passing = json!({"spec": {"replicas": 3}});
        let outcome = validating_admission_policy::evaluate(&def, "CREATE", "apps", "v1", "deployments", "", &BTreeMap::new(), &BTreeMap::new(), &[("object", &passing), ("request", &request)]);
        assert!(!outcome.denies());

        let failing = json!({"spec": {"replicas": 0}});
        let outcome = validating_admission_policy::evaluate(&def, "CREATE", "apps", "v1", "deployments", "", &BTreeMap::new(), &BTreeMap::new(), &[("object", &failing), ("request", &request)]);
        assert!(outcome.denies());

        // A namespace excluded by matchConditions never even reaches
        // validations, regardless of the object's own replicas.
        let excluded_request = json!({"namespace": "kube-system"});
        let outcome = validating_admission_policy::evaluate(&def, "CREATE", "apps", "v1", "deployments", "", &BTreeMap::new(), &BTreeMap::new(), &[("object", &failing), ("request", &excluded_request)]);
        assert_eq!(outcome, validating_admission_policy::PolicyOutcome::NotApplicable);
    }

    #[test]
    fn a_policy_with_no_match_constraints_at_all_decodes_to_empty_rules() {
        let policy = json!({"spec": {"validations": [{"expression": "true"}]}});
        let decoded = DecodedPolicy::decode(&policy);
        assert!(decoded.resource_rules().is_empty());
        assert!(decoded.exclude_resource_rules().is_empty());
        assert_eq!(decoded.validations.len(), 1);
    }

    #[test]
    fn failure_policy_defaults_to_fail_when_absent_or_unrecognized() {
        assert_eq!(DecodedPolicy::decode(&json!({"spec": {}})).failure_policy, FailurePolicy::Fail);
        assert_eq!(DecodedPolicy::decode(&json!({"spec": {"failurePolicy": "Bogus"}})).failure_policy, FailurePolicy::Fail);
        assert_eq!(DecodedPolicy::decode(&json!({"spec": {"failurePolicy": "Ignore"}})).failure_policy, FailurePolicy::Ignore);
    }

    #[test]
    fn a_null_selector_is_treated_the_same_as_an_absent_one() {
        let policy = json!({"spec": {"matchConstraints": {"namespaceSelector": null}}});
        let decoded = DecodedPolicy::decode(&policy);
        assert!(decoded.namespace_selector.is_none());
        assert!(policy_matching::matches_label_selector(decoded.namespace_selector, &BTreeMap::new()));
    }

    #[test]
    fn a_real_namespace_selector_decodes_and_matches_labels_correctly() {
        let policy = json!({"spec": {"matchConstraints": {"namespaceSelector": {"matchLabels": {"env": "prod"}}}}});
        let decoded = DecodedPolicy::decode(&policy);
        let prod: BTreeMap<String, String> = [("env".to_string(), "prod".to_string())].into();
        let dev: BTreeMap<String, String> = [("env".to_string(), "dev".to_string())].into();
        assert!(policy_matching::matches_label_selector(decoded.namespace_selector, &prod));
        assert!(!policy_matching::matches_label_selector(decoded.namespace_selector, &dev));
    }

    #[test]
    fn resource_rule_fields_decode_into_the_real_matching_shape() {
        let policy = json!({
            "spec": {
                "matchConstraints": {
                    "resourceRules": [{"apiGroups": ["*"], "apiVersions": ["v1"], "operations": ["CREATE", "UPDATE"], "resources": ["pods", "pods/status"]}],
                    "excludeResourceRules": [{"apiGroups": [""], "apiVersions": ["v1"], "operations": ["*"], "resources": ["configmaps"]}],
                },
            },
        });
        let decoded = DecodedPolicy::decode(&policy);
        let rules = decoded.resource_rules();
        assert_eq!(rules.len(), 1);
        assert!(policy_matching::resource_rule_matches(&rules[0], "CREATE", "apps", "v1", "pods", ""));
        assert!(policy_matching::resource_rule_matches(&rules[0], "UPDATE", "apps", "v1", "pods", "status"));
        assert!(!policy_matching::resource_rule_matches(&rules[0], "DELETE", "apps", "v1", "pods", ""));

        let excludes = decoded.exclude_resource_rules();
        assert_eq!(excludes.len(), 1);
        assert!(policy_matching::resource_rule_matches(&excludes[0], "CREATE", "", "v1", "configmaps", ""));
    }

    #[test]
    fn a_validation_missing_its_own_required_expression_is_skipped_not_treated_as_a_parse_failure() {
        let policy = json!({"spec": {"validations": [{"message": "no expression here"}, {"expression": "true"}]}});
        let decoded = DecodedPolicy::decode(&policy);
        assert_eq!(decoded.validations.len(), 1);
        assert_eq!(decoded.validations[0].expression, "true");
    }

    #[test]
    fn a_match_condition_missing_its_own_name_or_expression_is_skipped() {
        let policy = json!({"spec": {"matchConditions": [{"expression": "true"}, {"name": "only-name"}, {"name": "ok", "expression": "true"}]}});
        let decoded = DecodedPolicy::decode(&policy);
        assert_eq!(decoded.match_conditions.len(), 1);
        assert_eq!(decoded.match_conditions[0].name, "ok");
    }

    #[test]
    fn policy_variables_decode_in_the_declared_order() {
        let policy = json!({"spec": {"variables": [{"name": "replicas", "expression": "object.spec.replicas"}, {"name": "minimum", "expression": "variables.replicas + 2u"} ]}});
        let decoded = DecodedPolicy::decode(&policy);
        assert_eq!(decoded.variables.len(), 2);
        assert_eq!(decoded.variables[0].name, "replicas");
        assert_eq!(decoded.variables[1].expression, "variables.replicas + 2u");
    }
}
