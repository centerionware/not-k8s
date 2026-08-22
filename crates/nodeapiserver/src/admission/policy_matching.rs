//! `ValidatingAdmissionPolicy`'s own `spec.matchConstraints` matching
//! engine — the second real primitive [`super::match_conditions`]'s own doc comment
//! named as still-not-started: whether a request even reaches a policy's
//! `spec.matchConditions`/`spec.validations` at all. Same "land the
//! primitive first" discipline as `match_conditions` — this module has no
//! opinion on `ValidatingAdmissionPolicy` storage/CRUD, `ValidatingAdmission
//! PolicyBinding`, or `spec.paramRef` resolution; it answers exactly one
//! question, pure and I/O-free: does this `(operation, group, version,
//! resource, subresource)` request, against a namespace with these labels
//! and an object with these labels, match this policy's own declared
//! `matchConstraints`?
//!
//! Three real upstream pieces, ported separately since each is independently
//! testable and real upstream itself keeps them as separate functions:
//!
//! 1. [`resource_rule_matches`] / [`matches_resource_rules`] — real
//!    upstream's own `rules.Matcher` (`k8s.io/apiserver/pkg/admission/
//!    plugin/webhook/predicates/rules/rules.go`, fetched and read
//!    directly), the same `NamedRuleWithOperations`
//!    shape both webhooks and `ValidatingAdmissionPolicy` share for
//!    `resourceRules`/`excludeResourceRules`. **Named, honest gap**: real
//!    upstream's own `Rule.Scope` (`Namespaced`/`Cluster`/`*`) is not
//!    matched here — this crate's admission call sites don't carry a
//!    reliable "is this resource namespaced" signal alongside `Attributes`
//!    yet (`super::attributes`'s own doc comment names the same "don't
//!    build ahead of a real need" posture), so every rule is treated as if
//!    `scope` were `*` regardless of what the policy actually declared.
//! 2. [`label_selector_requirements`] / [`matches_label_selector`] — real
//!    upstream's own `metav1.LabelSelectorAsSelector`
//!    (`k8s.io/apimachinery/pkg/apis/meta/v1/helpers.go`): converts a real
//!    `LabelSelector`'s `matchLabels`/`matchExpressions` JSON shape into
//!    the same [`crate::cacher::selector::Requirement`] list
//!    `matches_labels` already evaluates — reused rather than
//!    reimplemented, since Group D's selector module is already a faithful
//!    port of the same real label-matching semantics
//!    `namespaceSelector`/`objectSelector` both need. A missing selector
//!    (`None`) matches everything, real upstream's own default.
//! 3. [`RequestVariable`] / [`build_request_object`] — real upstream's own
//!    `admission.Attributes` → CEL `request` variable construction
//!    (`k8s.io/apiserver/pkg/admission/plugin/cel/condition.go`'s
//!    `CreateAdmissionRequest`, fetched and read directly), the actual `request` binding
//!    `match_conditions`'s own doc comment named as still not built. Scoped
//!    to the fields this crate can honestly populate from
//!    [`super::attributes::Attributes`] today: **not populated** —
//!    `requestKind`/`kind`'s own `kind` field (`Attributes` carries
//!    `resource`, not the object's `Kind` string) and `userInfo` (this
//!    crate's admission call sites don't thread a real authenticated
//!    identity down to the admission layer yet, `authn`'s own module is
//!    wired at the handler-chain level above admission) — both named here
//!    rather than silently defaulted to a value that would look real but
//!    isn't.
//!
//! **Not yet wired to anything real**, same posture `match_conditions`
//! itself still carries: this crate has no `ValidatingAdmissionPolicy`/
//! `ValidatingAdmissionPolicyBinding` CRUD wiring or `spec.paramRef`
//! resolution yet (`docs/APISERVER.md`'s own Group J section names this),
//! so nothing calls this module from `server::listener` today — landed as
//! the standalone, pure matching decision the eventual enforcement call
//! site will need, exactly the shape `resource_quota`'s own evaluators
//! were landed in before `server::listener` wired them in.

use crate::cacher::selector::{self, Operator, Requirement};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// One real `v1.NamedRuleWithOperations` entry — the same shape
/// `resourceRules`/`excludeResourceRules` both use. `resources` follows
/// real upstream's own `"resource"`/`"resource/subresource"`/`"*"`/`"*/*"`
/// grammar (`split_resource` below).
#[derive(Debug, Clone, Copy)]
pub struct ResourceRule<'a> {
    pub operations: &'a [&'a str],
    pub api_groups: &'a [&'a str],
    pub api_versions: &'a [&'a str],
    pub resources: &'a [&'a str],
}

/// Real upstream's own `Matcher.Matches` minus the `scope()` check (see
/// this module's own doc comment) — every one of `operation()`/`group()`/
/// `version()`/`resource()` must hold.
pub fn resource_rule_matches(rule: &ResourceRule, operation: &str, group: &str, version: &str, resource: &str, subresource: &str) -> bool {
    wildcard_matches(rule.operations, operation) && wildcard_matches(rule.api_groups, group) && wildcard_matches(rule.api_versions, version) && resource_matches(rule.resources, resource, subresource)
}

fn wildcard_matches(values: &[&str], actual: &str) -> bool {
    values.iter().any(|v| *v == "*" || *v == actual)
}

/// Real upstream's own `resource()`: each declared entry splits into its
/// own resource/subresource halves, and each half matches independently —
/// `"pods/*"` matches every subresource of `pods`, **including the bare
/// resource itself** (`subresource == ""` is itself a value `"*"` matches,
/// real upstream draws no special exception for it), `"*/status"` matches
/// every resource's `status` subresource, and `"*/*"` matches anything at
/// all — bare resource included.
fn resource_matches(resources: &[&str], resource: &str, subresource: &str) -> bool {
    resources.iter().any(|r| {
        let (res, sub) = split_resource(r);
        (res == "*" || res == resource) && (sub == "*" || sub == subresource)
    })
}

fn split_resource(r: &str) -> (&str, &str) {
    match r.split_once('/') {
        Some((res, sub)) => (res, sub),
        None => (r, ""),
    }
}

/// Real upstream's own `matchesResourceRules`: a request matches
/// `spec.matchConstraints` when it matches **any** `resourceRules` entry
/// and **no** `excludeResourceRules` entry — real upstream's own
/// include-then-exclude ordering, not a single merged rule set. An empty
/// `resource_rules` slice never matches anything (real upstream requires
/// `matchConstraints.resourceRules` to be non-empty at policy-creation
/// time; this function makes the same "no rules, no match" call rather
/// than treating absence as "match everything").
pub fn matches_resource_rules(resource_rules: &[ResourceRule], exclude_rules: &[ResourceRule], operation: &str, group: &str, version: &str, resource: &str, subresource: &str) -> bool {
    let included = resource_rules.iter().any(|r| resource_rule_matches(r, operation, group, version, resource, subresource));
    included && !exclude_rules.iter().any(|r| resource_rule_matches(r, operation, group, version, resource, subresource))
}

/// Real upstream's own `metav1.LabelSelectorAsSelector`: `matchLabels`
/// entries become `Equals` requirements, each `matchExpressions` entry
/// becomes the [`Requirement`] its own `operator` names — an expression
/// with an operator this crate doesn't recognize is skipped rather than
/// treated as a parse failure, matching this module's pure/no-`Result`
/// shape (a policy admitted with a malformed selector is a validation gap
/// elsewhere, not this function's own concern).
pub fn label_selector_requirements(selector: &Value) -> Vec<Requirement> {
    let mut reqs = Vec::new();
    if let Some(match_labels) = selector.get("matchLabels").and_then(Value::as_object) {
        for (k, v) in match_labels {
            if let Some(v) = v.as_str() {
                reqs.push(Requirement { key: k.clone(), operator: Operator::Equals, values: vec![v.to_string()] });
            }
        }
    }
    if let Some(exprs) = selector.get("matchExpressions").and_then(Value::as_array) {
        for expr in exprs {
            let Some(key) = expr.get("key").and_then(Value::as_str) else { continue };
            let Some(op) = expr.get("operator").and_then(Value::as_str) else { continue };
            let operator = match op {
                "In" => Operator::In,
                "NotIn" => Operator::NotIn,
                "Exists" => Operator::Exists,
                "DoesNotExist" => Operator::DoesNotExist,
                _ => continue,
            };
            let values = expr.get("values").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            reqs.push(Requirement { key: key.to_string(), operator, values });
        }
    }
    reqs
}

/// `namespaceSelector`/`objectSelector`, both real upstream's own
/// `*metav1.LabelSelector` — `None` (the field absent from the policy)
/// matches everything, real upstream's own default-empty-selector
/// behavior (an explicit `{}` also matches everything, since
/// [`label_selector_requirements`] returns no requirements for it either
/// way — real upstream draws the same distinction: it's the field being
/// entirely absent vs. present-but-empty that differ in Go's zero-value
/// semantics, not in the resulting match decision).
pub fn matches_label_selector(selector: Option<&Value>, labels: &BTreeMap<String, String>) -> bool {
    match selector {
        None => true,
        Some(sel) => selector::matches_labels(&label_selector_requirements(sel), labels),
    }
}

/// The fields of real upstream's own `admission.Attributes` this crate can
/// honestly populate today — see this module's own doc comment for what's
/// named as not yet real (`kind`, `userInfo`).
#[derive(Debug, Clone, Copy)]
pub struct RequestVariable<'a> {
    pub uid: &'a str,
    pub group: &'a str,
    pub version: &'a str,
    pub resource: &'a str,
    pub subresource: &'a str,
    pub namespace: &'a str,
    pub name: &'a str,
    pub operation: &'a str,
    pub dry_run: bool,
}

/// Real upstream's own `CreateAdmissionRequest`'s JSON shape — the CEL
/// `request` variable a `ValidatingAdmissionPolicy`/webhook `matchCondition`
/// or `validations` rule binds. `kind`/`requestKind` are always emitted
/// with an empty `kind` field (see this module's own doc comment); the same
/// `resource`/`requestResource` are used for both `resource`/`requestResource`
/// since this crate has no notion yet of a request resource differing from
/// the resolved resource (real upstream's own distinction only matters
/// once CRD conversion is in play).
pub fn build_request_object(r: &RequestVariable) -> Value {
    let gvk = json!({"group": r.group, "version": r.version, "kind": ""});
    let gvr = json!({"group": r.group, "version": r.version, "resource": r.resource});
    json!({
        "uid": r.uid,
        "kind": gvk,
        "resource": gvr,
        "subResource": r.subresource,
        "requestKind": gvk,
        "requestResource": gvr,
        "requestSubResource": r.subresource,
        "name": r.name,
        "namespace": r.namespace,
        "operation": r.operation,
        "userInfo": {},
        "dryRun": r.dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_rule_matching_every_field_exactly_matches() {
        let rule = ResourceRule { operations: &["CREATE"], api_groups: &["apps"], api_versions: &["v1"], resources: &["deployments"] };
        assert!(resource_rule_matches(&rule, "CREATE", "apps", "v1", "deployments", ""));
    }

    #[test]
    fn a_wildcard_operation_matches_any_real_operation() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["apps"], api_versions: &["v1"], resources: &["deployments"] };
        assert!(resource_rule_matches(&rule, "DELETE", "apps", "v1", "deployments", ""));
    }

    #[test]
    fn an_operation_not_listed_does_not_match() {
        let rule = ResourceRule { operations: &["CREATE"], api_groups: &["apps"], api_versions: &["v1"], resources: &["deployments"] };
        assert!(!resource_rule_matches(&rule, "UPDATE", "apps", "v1", "deployments", ""));
    }

    #[test]
    fn wildcard_group_and_version_match_anything() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["pods"] };
        assert!(resource_rule_matches(&rule, "CREATE", "", "v1", "pods", ""));
        assert!(resource_rule_matches(&rule, "CREATE", "apps", "v1beta1", "pods", ""));
    }

    #[test]
    fn a_bare_resource_entry_matches_only_the_top_level_resource_no_subresource() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["pods"] };
        assert!(resource_rule_matches(&rule, "CREATE", "", "v1", "pods", ""));
        assert!(!resource_rule_matches(&rule, "CREATE", "", "v1", "pods", "status"));
    }

    #[test]
    fn an_explicit_subresource_entry_matches_only_that_subresource() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["pods/status"] };
        assert!(resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", "status"));
        assert!(!resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", ""));
        assert!(!resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", "scale"));
    }

    #[test]
    fn a_wildcard_subresource_entry_matches_any_subresource_and_the_bare_resource_too() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["pods/*"] };
        assert!(resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", "status"));
        assert!(resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", "scale"));
        assert!(resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", ""), "real upstream's own resource() draws no exception for the bare resource — \"*\" matches subresource==\"\" like any other value");
    }

    #[test]
    fn double_wildcard_matches_any_resource_any_subresource_bare_included() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*/*"] };
        assert!(resource_rule_matches(&rule, "UPDATE", "apps", "v1", "deployments", "status"));
        assert!(resource_rule_matches(&rule, "UPDATE", "apps", "v1", "deployments", ""));
    }

    #[test]
    fn included_by_resource_rules_but_also_excluded_does_not_match() {
        let include = [ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*"] }];
        let exclude = [ResourceRule { operations: &["*"], api_groups: &[""], api_versions: &["v1"], resources: &["configmaps"] }];
        assert!(matches_resource_rules(&include, &exclude, "CREATE", "apps", "v1", "deployments", ""));
        assert!(!matches_resource_rules(&include, &exclude, "CREATE", "", "v1", "configmaps", ""));
    }

    #[test]
    fn no_resource_rules_at_all_never_matches() {
        assert!(!matches_resource_rules(&[], &[], "CREATE", "apps", "v1", "deployments", ""));
    }

    #[test]
    fn label_selector_requirements_converts_match_labels_to_equals() {
        let sel = json!({"matchLabels": {"env": "prod"}});
        let reqs = label_selector_requirements(&sel);
        assert_eq!(reqs, vec![Requirement { key: "env".into(), operator: Operator::Equals, values: vec!["prod".into()] }]);
    }

    #[test]
    fn label_selector_requirements_converts_match_expressions() {
        let sel = json!({"matchExpressions": [{"key": "tier", "operator": "In", "values": ["frontend", "backend"]}]});
        let reqs = label_selector_requirements(&sel);
        assert_eq!(reqs, vec![Requirement { key: "tier".into(), operator: Operator::In, values: vec!["frontend".into(), "backend".into()] }]);
    }

    #[test]
    fn label_selector_requirements_combines_both_halves() {
        let sel = json!({
            "matchLabels": {"env": "prod"},
            "matchExpressions": [{"key": "tier", "operator": "Exists"}],
        });
        let reqs = label_selector_requirements(&sel);
        assert_eq!(reqs.len(), 2);
    }

    #[test]
    fn an_absent_selector_matches_every_namespace_or_object() {
        assert!(matches_label_selector(None, &labels(&[])));
        assert!(matches_label_selector(None, &labels(&[("any", "thing")])));
    }

    #[test]
    fn an_explicit_empty_selector_also_matches_everything() {
        assert!(matches_label_selector(Some(&json!({})), &labels(&[])));
    }

    #[test]
    fn a_real_selector_only_matches_labels_that_satisfy_it() {
        let sel = json!({"matchLabels": {"env": "prod"}});
        assert!(matches_label_selector(Some(&sel), &labels(&[("env", "prod")])));
        assert!(!matches_label_selector(Some(&sel), &labels(&[("env", "dev")])));
        assert!(!matches_label_selector(Some(&sel), &labels(&[])));
    }

    #[test]
    fn build_request_object_carries_the_real_operation_and_identity_fields() {
        let r = RequestVariable { uid: "abc-123", group: "apps", version: "v1", resource: "deployments", subresource: "", namespace: "default", name: "web", operation: "CREATE", dry_run: false };
        let obj = build_request_object(&r);
        assert_eq!(obj["uid"], json!("abc-123"));
        assert_eq!(obj["operation"], json!("CREATE"));
        assert_eq!(obj["namespace"], json!("default"));
        assert_eq!(obj["name"], json!("web"));
        assert_eq!(obj["resource"], json!({"group": "apps", "version": "v1", "resource": "deployments"}));
        assert_eq!(obj["dryRun"], json!(false));
    }

    #[test]
    fn build_request_object_carries_the_real_subresource_on_both_shapes() {
        let r = RequestVariable { uid: "x", group: "", version: "v1", resource: "pods", subresource: "status", namespace: "default", name: "p", operation: "UPDATE", dry_run: false };
        let obj = build_request_object(&r);
        assert_eq!(obj["subResource"], json!("status"));
        assert_eq!(obj["requestSubResource"], json!("status"));
    }
}
