//! The RBAC rule-matching primitive: given a `PolicyRule` (the shape a
//! `Role`/`ClusterRole` carries) and a request's attributes, decide
//! whether that one rule permits it. A faithful, line-by-line port of
//! real upstream's own matching functions (fetched and read directly, not
//! reconstructed from memory):
//! `pkg/apis/rbac/v1/evaluation_helpers.go`'s `VerbMatches`/
//! `APIGroupMatches`/`ResourceMatches`/`ResourceNameMatches`/
//! `NonResourceURLMatches`, composed by
//! `plugin/pkg/auth/authorizer/rbac/rbac.go`'s `RuleAllows` — including
//! the `*/subresource` wildcard `ResourceMatches` supports (a rule naming
//! `"*/status"` matches `pods/status`, `deployments/status`, ... without
//! naming every resource), and `NonResourceURLMatches`'s prefix-wildcard
//! rule (a rule ending in `*` matches any path with that prefix, not just
//! an exact string).
//!
//! # What this is, deliberately, and isn't yet
//!
//! This is the evaluation engine's core — genuinely reusable regardless
//! of where the `PolicyRule`s themselves come from. Real upstream builds
//! the candidate rule list by resolving `RoleBinding`/`ClusterRoleBinding`
//! objects for a subject (`ClusterRoleBindings` first, short-circuit on
//! match, then `RoleBinding`s in the request's namespace, deny by
//! default — the doc comment on real upstream's own `PolicyRule` type
//! states this evaluation order explicitly). That resolution — fetching
//! and aggregating real `Role`/`RoleBinding`/`ClusterRole`/
//! `ClusterRoleBinding` objects from storage for a given subject — is
//! separate, not-yet-started work; this module has no dependency on
//! storage or any real binding object at all, matching this crate's
//! established "land the primitive, wire it later" split.

/// `pkg/apis/rbac/v1/types.go`'s `PolicyRule`. `String` fields, not
/// `&'static str` — real rules come from live `Role`/`ClusterRole`
/// objects fetched at request time, not a compiled-in table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyRule {
    pub verbs: Vec<String>,
    pub api_groups: Vec<String>,
    pub resources: Vec<String>,
    pub resource_names: Vec<String>,
    pub non_resource_urls: Vec<String>,
}

/// The real upstream `"*"` wildcard constants — one spelling, reused for
/// verbs/apiGroups/resources/nonResourceURLs, matching
/// `rbacv1.VerbAll`/`APIGroupAll`/`ResourceAll`/`NonResourceAll` (all four
/// happen to be the literal string `"*"` upstream too, but each is its
/// own named constant there — kept separate here for the same reason:
/// a future upstream change to one must not silently change the others).
pub const VERB_ALL: &str = "*";
pub const API_GROUP_ALL: &str = "*";
pub const RESOURCE_ALL: &str = "*";
pub const NON_RESOURCE_ALL: &str = "*";

/// The parts of a request `RuleAllows` actually reads — deliberately not
/// `server::path::RequestInfo` itself (that carries HTTP-path-grammar
/// concerns like `parts`/`field_selector` this evaluator has no use for);
/// a caller building this from a real `RequestInfo` is expected to do
/// that translation itself.
#[derive(Debug, Clone, Default)]
pub struct RequestAttributes<'a> {
    pub is_resource_request: bool,
    pub verb: &'a str,
    pub api_group: &'a str,
    pub resource: &'a str,
    pub subresource: &'a str,
    pub name: &'a str,
    /// Only meaningful when `is_resource_request` is `false` — the
    /// non-resource URL path (`/healthz`, `/openapi/v3`, ...).
    pub path: &'a str,
}

pub fn verb_matches(rule: &PolicyRule, requested_verb: &str) -> bool {
    rule.verbs.iter().any(|v| v == VERB_ALL || v == requested_verb)
}

pub fn api_group_matches(rule: &PolicyRule, requested_group: &str) -> bool {
    rule.api_groups.iter().any(|g| g == API_GROUP_ALL || g == requested_group)
}

/// `combined_resource` is `resource` alone, or `resource/subresource` —
/// matching upstream's own `RuleAllows` building `combinedResource`
/// before calling this (kept as a separate parameter here, same as
/// upstream, rather than recomputed inside, so a caller with an
/// already-combined string doesn't have to split it back apart).
pub fn resource_matches(rule: &PolicyRule, combined_resource: &str, requested_subresource: &str) -> bool {
    for rule_resource in &rule.resources {
        if rule_resource == RESOURCE_ALL {
            return true;
        }
        if rule_resource == combined_resource {
            return true;
        }
        if requested_subresource.is_empty() {
            continue;
        }
        // A `*/<subresource>` rule matches any resource's identically
        // named subresource — the length check plus prefix/suffix check
        // together are exactly upstream's own way of confirming the rule
        // is *precisely* `"*/" + requested_subresource` (not, say, a rule
        // that merely happens to end with the same characters).
        if rule_resource.len() == requested_subresource.len() + 2 && rule_resource.starts_with("*/") && rule_resource.ends_with(requested_subresource) {
            return true;
        }
    }
    false
}

/// An empty `resource_names` list means "applies to every name" — real
/// upstream's own documented convention (`PolicyRule.ResourceNames`'s doc
/// comment: "An empty set means that everything is allowed").
pub fn resource_name_matches(rule: &PolicyRule, requested_name: &str) -> bool {
    rule.resource_names.is_empty() || rule.resource_names.iter().any(|n| n == requested_name)
}

pub fn non_resource_url_matches(rule: &PolicyRule, requested_url: &str) -> bool {
    for rule_url in &rule.non_resource_urls {
        if rule_url == NON_RESOURCE_ALL {
            return true;
        }
        if rule_url == requested_url {
            return true;
        }
        if let Some(prefix) = rule_url.strip_suffix('*') {
            if requested_url.starts_with(prefix) {
                return true;
            }
        }
    }
    false
}

/// `RuleAllows`: the one rule this request attributes matches, if any of
/// its component checks would let it through. Resource and non-resource
/// requests take entirely separate paths, matching upstream's own
/// `if requestAttributes.IsResourceRequest() { ... } else { ... }` split
/// (a `PolicyRule` combining both `resources` and `nonResourceURLs` is
/// real but unusual — upstream's own doc comment calls this out: "Rules
/// can either apply to API resources ... or non-resource URL paths ...
/// but not both" — this function doesn't need to special-case that
/// itself, since each branch only ever reads the fields relevant to it).
pub fn rule_allows(attrs: &RequestAttributes, rule: &PolicyRule) -> bool {
    if attrs.is_resource_request {
        let combined_resource = if attrs.subresource.is_empty() { attrs.resource.to_string() } else { format!("{}/{}", attrs.resource, attrs.subresource) };
        verb_matches(rule, attrs.verb) && api_group_matches(rule, attrs.api_group) && resource_matches(rule, &combined_resource, attrs.subresource) && resource_name_matches(rule, attrs.name)
    } else {
        verb_matches(rule, attrs.verb) && non_resource_url_matches(rule, attrs.path)
    }
}

/// `RulesAllow`: `true` if any rule in `rules` allows the request —
/// real upstream's own short-circuit-on-first-match evaluation, not "all
/// rules must agree."
pub fn rules_allow(attrs: &RequestAttributes, rules: &[PolicyRule]) -> bool {
    rules.iter().any(|r| rule_allows(attrs, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> PolicyRule {
        PolicyRule {
            verbs: vec!["get".to_string(), "list".to_string()],
            api_groups: vec!["".to_string()],
            resources: vec!["pods".to_string()],
            resource_names: vec![],
            non_resource_urls: vec![],
        }
    }

    fn attrs<'a>(verb: &'a str, resource: &'a str, name: &'a str) -> RequestAttributes<'a> {
        RequestAttributes { is_resource_request: true, verb, api_group: "", resource, subresource: "", name, path: "" }
    }

    #[test]
    fn a_matching_rule_allows() {
        assert!(rule_allows(&attrs("get", "pods", ""), &rule()));
        assert!(rule_allows(&attrs("list", "pods", ""), &rule()));
    }

    #[test]
    fn a_wrong_verb_or_resource_or_group_denies() {
        assert!(!rule_allows(&attrs("delete", "pods", ""), &rule()));
        assert!(!rule_allows(&attrs("get", "secrets", ""), &rule()));
        let mut r = rule();
        r.api_groups = vec!["apps".to_string()];
        assert!(!rule_allows(&attrs("get", "pods", ""), &r));
    }

    #[test]
    fn the_verb_all_wildcard_matches_any_verb() {
        let mut r = rule();
        r.verbs = vec![VERB_ALL.to_string()];
        assert!(rule_allows(&attrs("delete", "pods", ""), &r));
        assert!(rule_allows(&attrs("deletecollection", "pods", ""), &r));
    }

    #[test]
    fn the_resource_all_wildcard_matches_any_resource() {
        let mut r = rule();
        r.resources = vec![RESOURCE_ALL.to_string()];
        assert!(rule_allows(&attrs("get", "secrets", ""), &r));
    }

    #[test]
    fn an_empty_resource_names_list_allows_every_name() {
        assert!(rule_allows(&attrs("get", "pods", "any-name-at-all"), &rule()));
    }

    #[test]
    fn a_nonempty_resource_names_list_is_a_real_allowlist() {
        let mut r = rule();
        r.resource_names = vec!["web-1".to_string()];
        assert!(rule_allows(&attrs("get", "pods", "web-1"), &r));
        assert!(!rule_allows(&attrs("get", "pods", "web-2"), &r));
    }

    #[test]
    fn a_star_subresource_rule_matches_any_resources_matching_subresource() {
        let mut r = rule();
        r.resources = vec!["*/status".to_string()];
        let pod_status = RequestAttributes { is_resource_request: true, verb: "get", api_group: "", resource: "pods", subresource: "status", name: "", path: "" };
        let deployment_status = RequestAttributes { is_resource_request: true, verb: "get", api_group: "", resource: "deployments", subresource: "status", name: "", path: "" };
        assert!(rule_allows(&pod_status, &r));
        assert!(rule_allows(&deployment_status, &r));

        // A different subresource must not match.
        let pod_log = RequestAttributes { is_resource_request: true, verb: "get", api_group: "", resource: "pods", subresource: "log", name: "", path: "" };
        assert!(!rule_allows(&pod_log, &r));
    }

    #[test]
    fn a_plain_resource_rule_does_not_match_its_own_subresource() {
        // rule.resources = ["pods"] must NOT match "pods/status" -- a
        // subresource is a distinct combined resource string upstream
        // requires naming separately (or via */status).
        let pod_status = RequestAttributes { is_resource_request: true, verb: "get", api_group: "", resource: "pods", subresource: "status", name: "", path: "" };
        assert!(!rule_allows(&pod_status, &rule()));
    }

    #[test]
    fn non_resource_requests_use_the_path_not_the_resource_fields() {
        let r = PolicyRule { verbs: vec!["get".to_string()], non_resource_urls: vec!["/healthz".to_string()], ..Default::default() };
        let healthz = RequestAttributes { is_resource_request: false, verb: "get", path: "/healthz", ..Default::default() };
        assert!(rule_allows(&healthz, &r));

        let version = RequestAttributes { is_resource_request: false, verb: "get", path: "/version", ..Default::default() };
        assert!(!rule_allows(&version, &r));
    }

    #[test]
    fn a_trailing_star_non_resource_url_is_a_real_prefix_wildcard() {
        let r = PolicyRule { verbs: vec![VERB_ALL.to_string()], non_resource_urls: vec!["/openapi/*".to_string()], ..Default::default() };
        let v3 = RequestAttributes { is_resource_request: false, verb: "get", path: "/openapi/v3/apis/apps/v1", ..Default::default() };
        assert!(rule_allows(&v3, &r));

        let unrelated = RequestAttributes { is_resource_request: false, verb: "get", path: "/version", ..Default::default() };
        assert!(!rule_allows(&unrelated, &r));
    }

    #[test]
    fn rules_allow_short_circuits_on_the_first_matching_rule() {
        let deny_all = PolicyRule { verbs: vec![], ..Default::default() };
        let rules = vec![deny_all, rule()];
        assert!(rules_allow(&attrs("get", "pods", ""), &rules));
    }

    #[test]
    fn rules_allow_is_false_when_nothing_matches() {
        let rules = vec![rule()];
        assert!(!rules_allow(&attrs("delete", "secrets", ""), &rules));
    }

    #[test]
    fn an_empty_rule_list_denies_everything_deny_by_default() {
        assert!(!rules_allow(&attrs("get", "pods", ""), &[]));
    }
}
