//! `FlowSchema` matching — the first real piece of Group M's own API
//! Priority and Fairness (APF) subsystem, a faithful port of real
//! upstream's own `k8s.io/apiserver/pkg/util/flowcontrol/rule.go` +
//! `apihelpers.FlowSchemaSequence` (`pkg/util/apihelpers/helpers.go`),
//! release-1.34, fetched and read directly.
//!
//! This module remains the pure matching half. Storage resolution and the
//! request gate live in the sibling `resolve` and `limiter` modules; the
//! full upstream shuffle-sharded fair queue and seat-borrowing algorithm
//! remain separate refinements.
//!
//! # What's ported
//!
//! [`matches_flow_schema`]/[`matches_policy_rule`]: a `FlowSchema`
//! matches a request iff at least one of its `spec.rules` matches — a
//! rule matches iff its `subjects` matches the caller AND (depending on
//! whether the request is a resource request) at least one of its
//! `resourceRules`/`nonResourceRules` matches. [`matches_subject`] ports
//! all three real subject kinds (`User`/`Group`/`ServiceAccount`,
//! including the `ServiceAccount` wildcard-name case's own real
//! namespace-only prefix check, `serviceAccountMatchesNamespace` —
//! deliberately not simplified to reusing the exact-name matcher, same
//! as real upstream keeps the two as separate functions). Real
//! upstream's own `*` wildcard convention is ported for every list
//! (verbs/apiGroups/resources/nonResourceURLs/subjects' own `*` name),
//! including the non-resource URL's real prefix-match semantics
//! ([`matches_non_resource_url`] — `/foo/*` matches `/foo/`-prefixed
//! paths, `strip_suffix('*')` + ensure-trailing-slash, exactly upstream's
//! own `matchPolicyRuleNonResourceURL`; ported faithfully including its
//! own surprising real quirk that a rule with **no** trailing `*` still
//! falls through to the same trailing-slash prefix check once an exact
//! match fails — there's no "literal match only" case in real upstream's
//! own algorithm, confirmed directly from source rather than assumed).
//! [`select_flow_schema`]: `apihelpers.FlowSchemaSequence`'s own real
//! sort order — lowest `matchingPrecedence` wins (defaulting to real
//! upstream's own `1000` when unset), ties broken by lexicographically
//! smaller `name`.
//!
//! # Not ported
//!
//! The `distinguisherMethod`/flow-distinguisher computation (used only
//! for per-flow fairness once queuing exists, not for matching) and the
//! two mandatory bootstrap objects real upstream always synthesizes
//! (`exempt`/`catch-all` `FlowSchema`s) — this crate provisions no
//! bootstrap config objects of its own yet (Group O's job).

use serde_json::Value;

/// The subset of a request's identity/shape `FlowSchema` matching needs
/// — real upstream's own `RequestDigest` (`user.Info` + `*request.RequestInfo`),
/// narrowed to just the fields matching actually reads.
pub struct RequestDigest<'a> {
    pub user_name: &'a str,
    pub user_groups: &'a [String],
    pub verb: &'a str,
    pub is_resource_request: bool,
    pub api_group: &'a str,
    pub resource: &'a str,
    pub subresource: &'a str,
    pub namespace: &'a str,
    /// The raw request path — only consulted for a non-resource request,
    /// same as real upstream's own `ri.Path`.
    pub path: &'a str,
}

const ALL: &str = "*";

/// Real upstream's own `containsString`: `list` matches `x` if it's
/// exactly `[wildcard]` (validation elsewhere is what guarantees the
/// wildcard is never combined with other entries — this port doesn't
/// re-enforce that, same as it doesn't re-validate other object shapes)
/// or literally contains `x`.
fn contains_string(x: &str, list: &[&str], wildcard: &str) -> bool {
    if list.len() == 1 && list[0] == wildcard {
        return true;
    }
    list.contains(&x)
}

fn str_list<'a>(value: &'a Value, field: &str) -> Vec<&'a str> {
    value.get(field).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

/// `matchesSubject`/`serviceAccountMatchesNamespace`, ported exactly.
pub fn matches_subject(digest: &RequestDigest, subject: &Value) -> bool {
    match subject.get("kind").and_then(Value::as_str) {
        Some("User") => {
            let name = subject.get("user").and_then(|u| u.get("name")).and_then(Value::as_str);
            matches!(name, Some(n) if n == ALL || n == digest.user_name)
        }
        Some("Group") => {
            let Some(name) = subject.get("group").and_then(|g| g.get("name")).and_then(Value::as_str) else { return false };
            name == ALL || digest.user_groups.iter().any(|g| g == name)
        }
        Some("ServiceAccount") => {
            let Some(sa) = subject.get("serviceAccount") else { return false };
            let namespace = sa.get("namespace").and_then(Value::as_str).unwrap_or("");
            let name = sa.get("name").and_then(Value::as_str).unwrap_or("");
            if name == ALL {
                service_account_matches_namespace(namespace, digest.user_name)
            } else {
                crate::authz::subject::matches_service_account_username(namespace, name, digest.user_name)
            }
        }
        _ => false,
    }
}

/// Real upstream's own `serviceAccountMatchesNamespace`: the wildcard-name
/// `ServiceAccount` subject case checks only the `system:serviceaccount:<namespace>:`
/// prefix, not the actual name — deliberately a separate, narrower check
/// from [`crate::authz::subject::matches_service_account_username`]
/// rather than that function called with a name that always matches,
/// mirroring real upstream keeping these as two distinct functions.
fn service_account_matches_namespace(namespace: &str, username: &str) -> bool {
    const PREFIX: &str = "system:serviceaccount:";
    const SEPARATOR: &str = ":";
    let Some(rest) = username.strip_prefix(PREFIX) else { return false };
    let Some(rest) = rest.strip_prefix(namespace) else { return false };
    rest.starts_with(SEPARATOR)
}

fn matches_a_subject(digest: &RequestDigest, subjects: &Value) -> bool {
    subjects.as_array().into_iter().flatten().any(|s| matches_subject(digest, s))
}

fn matches_resource_policy_rule(digest: &RequestDigest, rule: &Value) -> bool {
    let verbs = str_list(rule, "verbs");
    if !contains_string(digest.verb, &verbs, ALL) {
        return false;
    }
    let resources = str_list(rule, "resources");
    let seek = if digest.subresource.is_empty() { digest.resource.to_string() } else { format!("{}/{}", digest.resource, digest.subresource) };
    if !contains_string(&seek, &resources, ALL) {
        return false;
    }
    let api_groups = str_list(rule, "apiGroups");
    if !contains_string(digest.api_group, &api_groups, ALL) {
        return false;
    }
    if digest.namespace.is_empty() {
        return rule.get("clusterScope").and_then(Value::as_bool).unwrap_or(false);
    }
    let namespaces = str_list(rule, "namespaces");
    contains_string(digest.namespace, &namespaces, "*")
}

fn matches_a_resource_rule(digest: &RequestDigest, rules: &Value) -> bool {
    rules.as_array().into_iter().flatten().any(|r| matches_resource_policy_rule(digest, r))
}

/// Real upstream's own `matchPolicyRuleNonResourceURL`: an exact match,
/// `*` matches everything, or a trailing-`*` prefix match (`/foo/*`
/// matches any path under `/foo/`, with or without the rule itself
/// having the trailing slash already).
fn matches_non_resource_url(rule_path: &str, request_path: &str) -> bool {
    if rule_path == ALL || rule_path == request_path {
        return true;
    }
    let prefix = rule_path.strip_suffix('*').unwrap_or(rule_path);
    let prefix = if prefix.ends_with('/') { prefix.to_string() } else { format!("{prefix}/") };
    request_path.starts_with(&prefix)
}

fn matches_non_resource_policy_rule(digest: &RequestDigest, rule: &Value) -> bool {
    let verbs = str_list(rule, "verbs");
    if !contains_string(digest.verb, &verbs, ALL) {
        return false;
    }
    str_list(rule, "nonResourceURLs").iter().any(|p| matches_non_resource_url(p, digest.path))
}

fn matches_a_non_resource_rule(digest: &RequestDigest, rules: &Value) -> bool {
    rules.as_array().into_iter().flatten().any(|r| matches_non_resource_policy_rule(digest, r))
}

/// `matchesPolicyRule`: one `spec.rules[]` entry (a real
/// `PolicyRulesWithSubjects`).
pub fn matches_policy_rule(digest: &RequestDigest, policy_rule: &Value) -> bool {
    let empty = Value::Array(vec![]);
    if !matches_a_subject(digest, policy_rule.get("subjects").unwrap_or(&empty)) {
        return false;
    }
    if digest.is_resource_request {
        matches_a_resource_rule(digest, policy_rule.get("resourceRules").unwrap_or(&empty))
    } else {
        matches_a_non_resource_rule(digest, policy_rule.get("nonResourceRules").unwrap_or(&empty))
    }
}

/// `matchesFlowSchema`: does any of `flow_schema.spec.rules` match?
pub fn matches_flow_schema(digest: &RequestDigest, flow_schema: &Value) -> bool {
    flow_schema.pointer("/spec/rules").and_then(Value::as_array).into_iter().flatten().any(|r| matches_policy_rule(digest, r))
}

/// Real upstream's own `FlowSchemaSpec.MatchingPrecedence` default —
/// "if the precedence is not specified, it will be set to 1000 as
/// default" (real upstream's own admission/defaulting does this on
/// write; this crate has no per-type defaulting wired for
/// `flowschemas` yet, so [`select_flow_schema`] applies the same
/// default at read time instead).
const DEFAULT_MATCHING_PRECEDENCE: i64 = 1000;

fn matching_precedence(flow_schema: &Value) -> i64 {
    flow_schema.pointer("/spec/matchingPrecedence").and_then(Value::as_i64).unwrap_or(DEFAULT_MATCHING_PRECEDENCE)
}

/// `apihelpers.FlowSchemaSequence`'s own real sort order: numerically
/// lowest `matchingPrecedence` first, ties broken by lexicographically
/// smaller `metadata.name`.
pub fn flow_schema_less(a: &Value, b: &Value) -> bool {
    let (pa, pb) = (matching_precedence(a), matching_precedence(b));
    if pa != pb {
        return pa < pb;
    }
    let name = |v: &Value| v.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("").to_string();
    name(a) < name(b)
}

/// Every `FlowSchema` in `flow_schemas` that matches `digest`, real
/// upstream's own `FlowSchemaSequence` order (lowest `matchingPrecedence`
/// first, name tie-break) — the first element, if any, is the one real
/// upstream's own filter would select.
pub fn select_flow_schema<'a>(flow_schemas: &'a [Value], digest: &RequestDigest) -> Option<&'a Value> {
    flow_schemas.iter().filter(|fs| matches_flow_schema(digest, fs)).min_by(|a, b| if flow_schema_less(a, b) { std::cmp::Ordering::Less } else if flow_schema_less(b, a) { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Equal })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest<'a>(user_name: &'a str, user_groups: &'a [String], verb: &'a str, resource: &'a str) -> RequestDigest<'a> {
        RequestDigest { user_name, user_groups, verb, is_resource_request: true, api_group: "", resource, subresource: "", namespace: "default", path: "" }
    }

    #[test]
    fn matches_subject_user_wildcard_and_exact() {
        let wildcard = json!({"kind": "User", "user": {"name": "*"}});
        let exact = json!({"kind": "User", "user": {"name": "alice"}});
        let d = digest("alice", &[], "get", "pods");
        assert!(matches_subject(&d, &wildcard));
        assert!(matches_subject(&d, &exact));
        assert!(!matches_subject(&digest("bob", &[], "get", "pods"), &exact));
    }

    #[test]
    fn matches_subject_group() {
        let groups = vec!["system:authenticated".to_string()];
        let d = digest("alice", &groups, "get", "pods");
        assert!(matches_subject(&d, &json!({"kind": "Group", "group": {"name": "system:authenticated"}})));
        assert!(matches_subject(&d, &json!({"kind": "Group", "group": {"name": "*"}})));
        assert!(!matches_subject(&d, &json!({"kind": "Group", "group": {"name": "system:masters"}})));
    }

    #[test]
    fn matches_subject_service_account_exact_and_wildcard_name() {
        let username = crate::authz::subject::service_account_username("kube-system", "coredns");
        let d = digest(&username, &[], "get", "pods");
        assert!(matches_subject(&d, &json!({"kind": "ServiceAccount", "serviceAccount": {"namespace": "kube-system", "name": "coredns"}})));
        assert!(matches_subject(&d, &json!({"kind": "ServiceAccount", "serviceAccount": {"namespace": "kube-system", "name": "*"}})));
        assert!(!matches_subject(&d, &json!({"kind": "ServiceAccount", "serviceAccount": {"namespace": "other-ns", "name": "*"}})));
        assert!(!matches_subject(&d, &json!({"kind": "ServiceAccount", "serviceAccount": {"namespace": "kube-system", "name": "other-sa"}})));
    }

    #[test]
    fn resource_rule_matches_verb_group_resource_and_namespace() {
        let rule = json!({"verbs": ["get", "list"], "apiGroups": [""], "resources": ["pods"], "namespaces": ["default"]});
        let d = digest("alice", &[], "get", "pods");
        assert!(matches_resource_policy_rule(&d, &rule));
        let wrong_verb = digest("alice", &[], "delete", "pods");
        assert!(!matches_resource_policy_rule(&wrong_verb, &rule));
    }

    #[test]
    fn resource_rule_cluster_scope_requires_no_namespace() {
        let rule = json!({"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"], "clusterScope": true});
        let mut d = digest("alice", &[], "get", "nodes");
        d.namespace = "";
        assert!(matches_resource_policy_rule(&d, &rule));
    }

    #[test]
    fn resource_rule_subresource_joins_with_slash() {
        let rule = json!({"verbs": ["*"], "apiGroups": ["*"], "resources": ["pods/status"], "namespaces": ["*"]});
        let mut d = digest("alice", &[], "get", "pods");
        d.subresource = "status";
        assert!(matches_resource_policy_rule(&d, &rule));
        d.subresource = "";
        assert!(!matches_resource_policy_rule(&d, &rule));
    }

    #[test]
    fn non_resource_url_prefix_match() {
        assert!(matches_non_resource_url("/healthz/*", "/healthz/ping"));
        assert!(matches_non_resource_url("/healthz*", "/healthz/ping"));
        assert!(matches_non_resource_url("*", "/anything"));
        assert!(matches_non_resource_url("/healthz", "/healthz"));
        // A surprising but real, verified-against-source upstream quirk:
        // `matchPolicyRuleNonResourceURL` always falls through to the
        // trailing-slash prefix check even when the rule carries no `*`
        // at all -- there's no "exact match only, no implicit wildcard"
        // case once the literal-equality check above has failed.
        assert!(matches_non_resource_url("/healthz", "/healthz/ping"));
        assert!(!matches_non_resource_url("/healthz", "/healthzz"));
    }

    #[test]
    fn matches_flow_schema_end_to_end() {
        let flow_schema = json!({
            "spec": {
                "rules": [{
                    "subjects": [{"kind": "Group", "group": {"name": "system:authenticated"}}],
                    "resourceRules": [{"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"], "namespaces": ["*"]}],
                }],
            },
        });
        let groups = vec!["system:authenticated".to_string()];
        let d = digest("alice", &groups, "get", "pods");
        assert!(matches_flow_schema(&d, &flow_schema));
        assert!(!matches_flow_schema(&digest("bob", &[], "get", "pods"), &flow_schema));
    }

    #[test]
    fn flow_schema_less_orders_by_precedence_then_name() {
        let low_precedence = json!({"metadata": {"name": "zzz"}, "spec": {"matchingPrecedence": 100}});
        let high_precedence = json!({"metadata": {"name": "aaa"}, "spec": {"matchingPrecedence": 900}});
        assert!(flow_schema_less(&low_precedence, &high_precedence), "lower matchingPrecedence wins regardless of name");

        let a = json!({"metadata": {"name": "aaa"}, "spec": {"matchingPrecedence": 500}});
        let b = json!({"metadata": {"name": "bbb"}, "spec": {"matchingPrecedence": 500}});
        assert!(flow_schema_less(&a, &b), "a tie in precedence is broken by lexicographically smaller name");
    }

    #[test]
    fn flow_schema_less_defaults_unset_precedence_to_1000() {
        let unset = json!({"metadata": {"name": "unset"}, "spec": {}});
        let explicit_1000 = json!({"metadata": {"name": "explicit"}, "spec": {"matchingPrecedence": 1000}});
        // Both land on precedence 1000 (unset defaults there too), so this
        // comes down entirely to the name tie-break: "explicit" < "unset".
        assert!(flow_schema_less(&explicit_1000, &unset));
        assert!(!flow_schema_less(&unset, &explicit_1000));
    }

    #[test]
    fn select_flow_schema_picks_the_real_winner() {
        let a = json!({"metadata": {"name": "a-schema"}, "spec": {"matchingPrecedence": 500, "rules": [{
            "subjects": [{"kind": "User", "user": {"name": "*"}}],
            "resourceRules": [{"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"], "namespaces": ["*"]}],
        }]}});
        let b = json!({"metadata": {"name": "b-schema"}, "spec": {"matchingPrecedence": 100, "rules": [{
            "subjects": [{"kind": "User", "user": {"name": "*"}}],
            "resourceRules": [{"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"], "namespaces": ["*"]}],
        }]}});
        let schemas = vec![a, b];
        let d = digest("alice", &[], "get", "pods");
        let selected = select_flow_schema(&schemas, &d).expect("both match, one should be selected");
        assert_eq!(selected.pointer("/metadata/name").unwrap(), "b-schema", "lower matchingPrecedence (100) should win over 500");
    }

    #[test]
    fn select_flow_schema_returns_none_when_nothing_matches() {
        let schemas = vec![json!({"metadata": {"name": "only"}, "spec": {"rules": [{
            "subjects": [{"kind": "User", "user": {"name": "only-this-user"}}],
            "resourceRules": [{"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"], "namespaces": ["*"]}],
        }]}})];
        let d = digest("someone-else", &[], "get", "pods");
        assert!(select_flow_schema(&schemas, &d).is_none());
    }
}
