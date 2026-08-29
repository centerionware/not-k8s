//! `ValidatingAdmissionPolicy`'s own `spec.matchConstraints` matching
//! engine. The storage-backed [`super::policy_enforcement`] adapter consumes
//! this primitive to decide whether a request reaches a policy's
//! `spec.matchConditions`/`spec.validations` at all. Same "land the
//! primitive first" discipline as `match_conditions` — this module has no
//! opinion on `ValidatingAdmissionPolicy` storage/CRUD, `ValidatingAdmission
//! PolicyBinding`, or `spec.paramRef` resolution; it answers exactly one
//! question, pure and I/O-free: does this `(operation, group, version,
//! resource, subresource)` request, against a namespace with these labels
//! and an object with these labels, match this policy's own declared
//! `matchConstraints`?
//!
//! Four real upstream pieces, ported separately since each is independently
//! testable and real upstream itself keeps them as separate functions:
//!
//! 1. [`resource_rule_matches`] / [`matches_resource_rules`] — real
//!    upstream's own `rules.Matcher` (`k8s.io/apiserver/pkg/admission/
//!    plugin/webhook/predicates/rules/rules.go`, fetched and read
//!    directly), the same `NamedRuleWithOperations`
//!    shape both webhooks and `ValidatingAdmissionPolicy` share for
//!    `resourceRules`/`excludeResourceRules`. **Named, honest gap**: real
//!    upstream's own `Rule.Scope` (`Namespaced`/`Cluster`/`*`) is matched
//!    when the storage-backed adapter supplies the discovered resource scope.
//!    The pure compatibility wrapper remains scope-agnostic when its caller
//!    has no discovery context.
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
//!    `requestKind`/`kind` and `userInfo` are populated by the storage-backed
//!    adapter from the submitted/old object and authenticated identity.
//! 4. [`build_eval_vars`] — the real `object`/`oldObject`/`request`/
//!    `params` variable set `match_conditions`/`policy_validations` both
//!    expect their own `vars` slice to already carry, assembled from
//!    already-decoded JSON. `object`/`oldObject`/`params` bind to a real
//!    CEL `null` (not an absent variable) when the caller has none —
//!    real upstream's own real behavior, verified live by binding
//!    `Value::Null` through the exact same generic
//!    `cel_ext::eval_bool_with_vars` this whole arc already uses.
//! 5. [`compose_variables`] — the real `spec.variables` composition
//!    contract: evaluate declarations in order, expose prior results under
//!    `variables`, and keep the result as a JSON object that can be bound to
//!    later validation or mutation expressions.
//!
//! The storage-backed `policy_enforcement` adapter calls this module from
//! `server::listener` for real `ValidatingAdmissionPolicy` requests. This
//! module remains deliberately pure: policy CRUD and `spec.paramRef`
//! resolution belong to that adapter. The adapter binds the Kubernetes
//! `authorizer` CEL library from a request-local RBAC snapshot.

use crate::cacher::selector::{self, Operator, Requirement};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// One real `v1.NamedRuleWithOperations` entry — the same shape
/// `resourceRules`/`excludeResourceRules` both use. `resources` follows
/// real upstream's own `"resource"`/`"resource/subresource"`/`"*"`/`"*/*"`
/// grammar (`split_resource` below).
///
/// Two lifetimes, not one: `'a` is how long the four string-array slices
/// live, `'b` is how long the `&str` data they point at lives — deliberately
/// decoupled so `admission::policy_decode`'s own `DecodedResourceRule` can
/// hand out a `ResourceRule` borrowing its *own* backing `Vec<&'b str>`
/// storage (lifetime `'a`, tied to that one borrow) while the `&str`
/// values inside still point directly at the original decoded
/// `serde_json::Value` (lifetime `'b`, outliving `'a`) — a single shared
/// lifetime would force every caller to keep that intermediate `Vec`
/// storage alive exactly as long as the original JSON, which a decode
/// step has no reason to require.
#[derive(Debug, Clone, Copy)]
pub struct ResourceRule<'a, 'b> {
    pub operations: &'a [&'b str],
    pub api_groups: &'a [&'b str],
    pub api_versions: &'a [&'b str],
    pub resources: &'a [&'b str],
    /// `Namespaced`, `Cluster`, or `*`, matching the API's
    /// `NamedRuleWithOperations.scope` field.
    pub scope: &'b str,
}

/// Real upstream's own `Matcher.Matches` — every one of
/// `operation()`/`group()`/`version()`/`resource()` and, when supplied,
/// `scope()` must hold.
pub fn resource_rule_matches(rule: &ResourceRule, operation: &str, group: &str, version: &str, resource: &str, subresource: &str) -> bool {
    resource_rule_matches_with_scope(rule, operation, group, version, resource, subresource, None)
}

/// [`resource_rule_matches`] with the resource's discovered namespacedness.
/// `None` preserves the pure helper's historical scope-agnostic behavior.
pub fn resource_rule_matches_with_scope(rule: &ResourceRule, operation: &str, group: &str, version: &str, resource: &str, subresource: &str, namespaced: Option<bool>) -> bool {
    wildcard_matches(rule.operations, operation)
        && wildcard_matches(rule.api_groups, group)
        && wildcard_matches(rule.api_versions, version)
        && resource_matches(rule.resources, resource, subresource)
        && scope_matches(rule.scope, namespaced)
}

fn scope_matches(scope: &str, namespaced: Option<bool>) -> bool {
    match scope {
        "*" => true,
        "Namespaced" => namespaced == Some(true),
        "Cluster" => namespaced == Some(false),
        _ => false,
    }
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
    matches_resource_rules_with_scope(resource_rules, exclude_rules, operation, group, version, resource, subresource, None)
}

/// [`matches_resource_rules`] with the resolved resource scope.
pub fn matches_resource_rules_with_scope(resource_rules: &[ResourceRule], exclude_rules: &[ResourceRule], operation: &str, group: &str, version: &str, resource: &str, subresource: &str, namespaced: Option<bool>) -> bool {
    let included = resource_rules.iter().any(|r| resource_rule_matches_with_scope(r, operation, group, version, resource, subresource, namespaced));
    included && !exclude_rules.iter().any(|r| resource_rule_matches_with_scope(r, operation, group, version, resource, subresource, namespaced))
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

/// The request identity portion of real upstream's own `user.Info` shape.
/// Slices borrow the authenticated identity for the duration of request
/// construction; no identity data is retained after the CEL value is built.
#[derive(Debug, Clone, Copy)]
pub struct RequestUserInfo<'a> {
    pub username: &'a str,
    pub uid: Option<&'a str>,
    pub groups: &'a [String],
}

/// The fields of real upstream's own `admission.Attributes` used to build a
/// CEL `request` value.
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
    pub kind: &'a str,
    pub user_info: Option<RequestUserInfo<'a>>,
}

/// One real `ValidatingAdmissionPolicySpec.variables` entry. Variables are
/// evaluated in declaration order; an expression may read only entries that
/// precede it through the `variables` object.
#[derive(Debug, Clone, Copy)]
pub struct Variable<'a> {
    pub name: &'a str,
    pub expression: &'a str,
}

/// Real upstream's own `CreateAdmissionRequest`'s JSON shape — the CEL
/// `request` variable a `ValidatingAdmissionPolicy`/webhook `matchCondition`
/// or `validations` rule binds. `kind`/`requestKind` carry the submitted
/// object's kind; the same
/// `resource`/`requestResource` are used for both `resource`/`requestResource`
/// since this crate has no notion yet of a request resource differing from
/// the resolved resource (real upstream's own distinction only matters
/// once CRD conversion is in play).
pub fn build_request_object(r: &RequestVariable) -> Value {
    let gvk = json!({"group": r.group, "version": r.version, "kind": r.kind});
    let gvr = json!({"group": r.group, "version": r.version, "resource": r.resource});
    let user_info = match r.user_info {
        Some(user) => json!({"username": user.username, "uid": user.uid, "groups": user.groups, "extra": {}}),
        None => json!({"username": "system:anonymous", "groups": ["system:unauthenticated"], "extra": {}}),
    };
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
        "userInfo": user_info,
        "dryRun": r.dry_run,
    })
}

/// Real upstream's own `null` for a JSON value this crate's own generic
/// `serde_json::Value -> cel::Value` binding (`cel_ext::eval_bool_with_vars`'s
/// own `ctx.add_variable(name, value.clone())`) already handles the same
/// way it handles every other `Value` variant — used as the real bound
/// value for `object`/`oldObject`/`params` when the caller has none to
/// give, rather than leaving the CEL variable unbound (see
/// [`build_eval_vars`]'s own doc comment for why that distinction is
/// real, not cosmetic).
const NULL: Value = Value::Null;

/// Real upstream's own real `object`/`oldObject`/`request`/`params` CEL
/// variable set (`match_conditions`'s own doc comment names this as the
/// binding `admission::match_conditions`/`policy_validations` both expect
/// their `vars` slice to already carry) — assembled from already-decoded
/// JSON, not itself doing any decoding.
///
/// `object`/`oldObject`/`params` bind to a real CEL `null`
/// ([`NULL`]), not an *absent* variable, when the caller passes `None` —
/// matching real upstream's own real behavior: `object` is genuinely
/// `null` on `DELETE`, `oldObject` is genuinely `null` on `CREATE`, and
/// `params` is genuinely `null` when a policy declares no `paramKind` (or
/// a binding declares no `paramRef`) — an expression like `oldObject ==
/// null` is real, valid, and meant to evaluate `true` on `CREATE`, not
/// fail with an undefined-variable error.
///
/// `namespaceObject` is intentionally separate: it is available to
/// `spec.validations`, but not to `spec.matchConditions`, so callers that
/// evaluate both stages must use [`build_eval_vars`] for the match stage and
/// [`build_eval_vars_with_namespace`] for the validation stage. The composed
/// `spec.variables` map is added by [`compose_variables`] after matching.
/// The storage-backed adapter adds the CEL `authorizer` value after this
/// base set has been assembled.
pub fn build_eval_vars<'a>(object: Option<&'a Value>, old_object: Option<&'a Value>, request: &'a Value, params: Option<&'a Value>) -> Vec<(&'static str, &'a Value)> {
    vec![
        ("object", object.unwrap_or(&NULL)),
        ("oldObject", old_object.unwrap_or(&NULL)),
        ("request", request),
        ("params", params.unwrap_or(&NULL)),
    ]
}

pub fn build_eval_vars_with_namespace<'a>(object: Option<&'a Value>, old_object: Option<&'a Value>, request: &'a Value, params: Option<&'a Value>, namespace_object: Option<&'a Value>) -> Vec<(&'static str, &'a Value)> {
    vec![
        ("object", object.unwrap_or(&NULL)),
        ("oldObject", old_object.unwrap_or(&NULL)),
        ("request", request),
        ("params", params.unwrap_or(&NULL)),
        ("namespaceObject", namespace_object.unwrap_or(&NULL)),
    ]
}

/// Evaluate a policy's composed variables in their declared order and return
/// the resulting `variables` object. Each expression sees the request
/// variables plus a snapshot containing only earlier composed variables,
/// matching the API contract that later declarations cannot be referenced.
/// The existing request-side CEL deadline also bounds each composition step.
pub fn compose_variables(variables: &[Variable<'_>], base_vars: &[(&'static str, &Value)]) -> Result<Value, String> {
    compose_variables_with_cel_vars(variables, base_vars, &[])
}

/// [`compose_variables`] with native CEL values in addition to JSON values.
pub fn compose_variables_with_cel_vars(variables: &[Variable<'_>], base_vars: &[(&'static str, &Value)], cel_vars: &[(&'static str, cel::Value)]) -> Result<Value, String> {
    let mut values = serde_json::Map::new();
    for variable in variables {
        let available = Value::Object(values.clone());
        let mut eval_vars = base_vars.to_vec();
        eval_vars.push(("variables", &available));
        let value = crate::cel_ext::eval_json_with_vars_and_cel_vars_and_deadline(variable.expression, &eval_vars, cel_vars, std::time::Duration::from_millis(100))
            .map_err(|error| format!("composing policy variable {:?}: {error}", variable.name))?;
        values.insert(variable.name.to_string(), value);
    }
    Ok(Value::Object(values))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_rule_matching_every_field_exactly_matches() {
        let rule = ResourceRule { operations: &["CREATE"], api_groups: &["apps"], api_versions: &["v1"], resources: &["deployments"], scope: "*" };
        assert!(resource_rule_matches(&rule, "CREATE", "apps", "v1", "deployments", ""));
    }

    #[test]
    fn a_rule_scope_matches_the_discovered_resource_scope() {
        let namespaced = ResourceRule { operations: &["*"], api_groups: &["apps"], api_versions: &["v1"], resources: &["deployments"], scope: "Namespaced" };
        let cluster = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["v1"], resources: &["nodes"], scope: "Cluster" };
        assert!(resource_rule_matches_with_scope(&namespaced, "CREATE", "apps", "v1", "deployments", "", Some(true)));
        assert!(!resource_rule_matches_with_scope(&namespaced, "CREATE", "apps", "v1", "deployments", "", Some(false)));
        assert!(resource_rule_matches_with_scope(&cluster, "GET", "", "v1", "nodes", "", Some(false)));
    }

    #[test]
    fn a_wildcard_operation_matches_any_real_operation() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["apps"], api_versions: &["v1"], resources: &["deployments"], scope: "*" };
        assert!(resource_rule_matches(&rule, "DELETE", "apps", "v1", "deployments", ""));
    }

    #[test]
    fn an_operation_not_listed_does_not_match() {
        let rule = ResourceRule { operations: &["CREATE"], api_groups: &["apps"], api_versions: &["v1"], resources: &["deployments"], scope: "*" };
        assert!(!resource_rule_matches(&rule, "UPDATE", "apps", "v1", "deployments", ""));
    }

    #[test]
    fn wildcard_group_and_version_match_anything() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["pods"], scope: "*" };
        assert!(resource_rule_matches(&rule, "CREATE", "", "v1", "pods", ""));
        assert!(resource_rule_matches(&rule, "CREATE", "apps", "v1beta1", "pods", ""));
    }

    #[test]
    fn a_bare_resource_entry_matches_only_the_top_level_resource_no_subresource() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["pods"], scope: "*" };
        assert!(resource_rule_matches(&rule, "CREATE", "", "v1", "pods", ""));
        assert!(!resource_rule_matches(&rule, "CREATE", "", "v1", "pods", "status"));
    }

    #[test]
    fn an_explicit_subresource_entry_matches_only_that_subresource() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["pods/status"], scope: "*" };
        assert!(resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", "status"));
        assert!(!resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", ""));
        assert!(!resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", "scale"));
    }

    #[test]
    fn a_wildcard_subresource_entry_matches_any_subresource_and_the_bare_resource_too() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["pods/*"], scope: "*" };
        assert!(resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", "status"));
        assert!(resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", "scale"));
        assert!(resource_rule_matches(&rule, "UPDATE", "", "v1", "pods", ""), "real upstream's own resource() draws no exception for the bare resource — \"*\" matches subresource==\"\" like any other value");
    }

    #[test]
    fn double_wildcard_matches_any_resource_any_subresource_bare_included() {
        let rule = ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*/*"], scope: "*" };
        assert!(resource_rule_matches(&rule, "UPDATE", "apps", "v1", "deployments", "status"));
        assert!(resource_rule_matches(&rule, "UPDATE", "apps", "v1", "deployments", ""));
    }

    #[test]
    fn included_by_resource_rules_but_also_excluded_does_not_match() {
        let include = [ResourceRule { operations: &["*"], api_groups: &["*"], api_versions: &["*"], resources: &["*"], scope: "*" }];
        let exclude = [ResourceRule { operations: &["*"], api_groups: &[""], api_versions: &["v1"], resources: &["configmaps"], scope: "*" }];
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
        let r = RequestVariable { uid: "abc-123", group: "apps", version: "v1", resource: "deployments", subresource: "", namespace: "default", name: "web", operation: "CREATE", dry_run: false, kind: "Deployment", user_info: None };
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
        let r = RequestVariable { uid: "x", group: "", version: "v1", resource: "pods", subresource: "status", namespace: "default", name: "p", operation: "UPDATE", dry_run: false, kind: "Pod", user_info: None };
        let obj = build_request_object(&r);
        assert_eq!(obj["subResource"], json!("status"));
        assert_eq!(obj["requestSubResource"], json!("status"));
    }

    #[test]
    fn build_request_object_carries_kind_and_authenticated_user_info() {
        let groups = vec!["developers".to_string()];
        let r = RequestVariable {
            uid: "request-id",
            group: "apps",
            version: "v1",
            resource: "deployments",
            subresource: "",
            namespace: "default",
            name: "web",
            operation: "CREATE",
            dry_run: false,
            kind: "Deployment",
            user_info: Some(RequestUserInfo { username: "alice", uid: Some("user-id"), groups: &groups }),
        };
        let obj = build_request_object(&r);
        assert_eq!(obj["kind"]["kind"], json!("Deployment"));
        assert_eq!(obj["requestKind"]["kind"], json!("Deployment"));
        assert_eq!(obj["userInfo"], json!({"username": "alice", "uid": "user-id", "groups": ["developers"], "extra": {}}));
    }

    #[test]
    fn build_eval_vars_binds_every_given_value_under_its_own_real_name() {
        let object = json!({"replicas": 3});
        let old_object = json!({"replicas": 1});
        let request = json!({"operation": "UPDATE"});
        let params = json!({"max": 5});
        let vars = build_eval_vars(Some(&object), Some(&old_object), &request, Some(&params));
        let names: Vec<&str> = vars.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, vec!["object", "oldObject", "request", "params"]);
        assert_eq!(vars[0].1, &object);
        assert_eq!(vars[1].1, &old_object);
        assert_eq!(vars[2].1, &request);
        assert_eq!(vars[3].1, &params);
    }

    #[test]
    fn build_eval_vars_binds_a_real_cel_null_not_an_absent_variable_when_object_is_none() {
        let request = json!({"operation": "DELETE"});
        let old_object = json!({"replicas": 1});
        let vars = build_eval_vars(None, Some(&old_object), &request, None);
        // A real expression comparing the absent variables to `null` must
        // actually evaluate, not fail with an undefined-variable error --
        // proving these are bound to a real CEL null, not omitted.
        assert_eq!(crate::cel_ext::eval_bool_with_vars("object == null", &vars).unwrap(), true);
        assert_eq!(crate::cel_ext::eval_bool_with_vars("params == null", &vars).unwrap(), true);
        assert_eq!(crate::cel_ext::eval_bool_with_vars("oldObject.replicas == 1", &vars).unwrap(), true);
    }

    #[test]
    fn build_eval_vars_binds_a_real_cel_null_for_old_object_on_create() {
        let object = json!({"replicas": 1});
        let request = json!({"operation": "CREATE"});
        let vars = build_eval_vars(Some(&object), None, &request, None);
        assert_eq!(crate::cel_ext::eval_bool_with_vars("oldObject == null", &vars).unwrap(), true);
        assert_eq!(crate::cel_ext::eval_bool_with_vars("object.replicas == 1", &vars).unwrap(), true);
    }

    #[test]
    fn build_eval_vars_with_namespace_binds_the_namespace_object() {
        let object = json!({"name": "pod"});
        let request = json!({"operation": "CREATE"});
        let namespace = json!({"metadata": {"name": "default"}});
        let vars = build_eval_vars_with_namespace(Some(&object), None, &request, None, Some(&namespace));
        assert_eq!(crate::cel_ext::eval_bool_with_vars("namespaceObject.metadata.name == 'default'", &vars).unwrap(), true);
    }

    #[test]
    fn composed_variables_are_available_in_declaration_order() {
        let object = json!({"spec": {"replicas": 3}});
        let request = json!({"operation": "CREATE"});
        let variables = [
            Variable { name: "replicas", expression: "object.spec.replicas" },
            Variable { name: "minimum", expression: "variables.replicas + 2u" },
        ];
        let base = build_eval_vars(Some(&object), None, &request, None);
        let composed = compose_variables(&variables, &base).unwrap();
        assert_eq!(composed["replicas"], json!(3));
        assert_eq!(composed["minimum"], json!(5));
    }

    #[test]
    fn composed_variables_cannot_reference_a_later_declaration() {
        let request = json!({"operation": "CREATE"});
        let variables = [
            Variable { name: "first", expression: "variables.second" },
            Variable { name: "second", expression: "1" },
        ];
        let base = build_eval_vars(None, None, &request, None);
        assert!(compose_variables(&variables, &base).is_err());
    }
}
