//! `SubjectAccessReview`/`SelfSubjectAccessReview` — a faithful wiring of
//! this crate's already-built RBAC engine
//! (`authz::resolve::rules_for` + `authz::rbac::rules_allow`) to real
//! upstream's own `authorization.k8s.io/v1` review API
//! (`k8s.io/api/authorization/v1/types.go`, fetched and read directly).
//! No new evaluation logic at all — this is purely a *virtual resource*:
//! a `POST` that computes a decision and hands it back in the response's
//! own `status`, never persisted to storage (real upstream's own
//! `SubjectAccessReview` isn't backed by etcd storage either — it's a
//! synthetic REST connector, `pkg/registry/authorization/subjectaccessreview`).
//!
//! [`parse_spec`] reads a `SubjectAccessReviewSpec`: exactly one of
//! `resourceAttributes`/`nonResourceAttributes` (real upstream's own
//! requirement — `pkg/apis/authorization/validation/validation.go`'s
//! `ValidateSubjectAccessReviewSpec`), `user`/`groups` from the spec
//! itself for `SubjectAccessReview` or from the caller's own verified
//! identity for `SelfSubjectAccessReview` (`server::listener` passes the
//! right fallback for each). [`build_status`] renders the real
//! `SubjectAccessReviewStatus` shape.
//!
//! [`build_rules_status`] is `SelfSubjectRulesReview`'s own real
//! `SubjectRulesReviewStatus` shape — every already-resolved
//! `PolicyRule` for one namespace, split into `resourceRules`/
//! `nonResourceRules` by which fields each rule actually names.
//!
//! `LocalSubjectAccessReview` (the namespaced variant — the namespace
//! comes from the URL, not the body) shares [`parse_spec`]/
//! [`build_status`] unchanged; `server::listener`'s own branch overrides
//! the parsed `namespace` with the URL's own after parsing.
//!
//! **Named, honest scope**: `evaluationError`/per-rule `reason` aren't
//! populated on [`build_status`] (this crate's RBAC engine doesn't track
//! *which* rule matched, only whether any did) and `denied` is never set
//! — real RBAC's own authorizer (`plugin/pkg/auth/authorizer/rbac/rbac.go`)
//! never returns an explicit `DecisionDeny` either, only `DecisionAllow`/
//! `DecisionNoOpinion`, so omitting `denied` here matches real RBAC's own
//! behavior, not a shortcut.

use serde_json::Value;

/// One resolved review request — either a resource or non-resource
/// check, matching real upstream's own `authorizer.Attributes` shape
/// closely enough to hand straight to `authz::rbac::RequestAttributes`.
pub struct Request {
    pub user_name: String,
    pub user_groups: Vec<String>,
    pub is_resource: bool,
    pub namespace: String,
    pub verb: String,
    pub group: String,
    pub resource: String,
    pub subresource: String,
    pub name: String,
    pub path: String,
}

/// Parses a `SubjectAccessReviewSpec`/`SelfSubjectAccessReviewSpec` JSON
/// body. `fallback_user`/`fallback_groups` are used when the spec itself
/// carries no `user`/`groups` (always the case for
/// `SelfSubjectAccessReview`, whose real schema has no such fields at
/// all — the caller's own verified identity is authoritative there;
/// `SubjectAccessReview`'s schema does carry them, for asking about a
/// *different* subject, and they take precedence when present).
pub fn parse_spec(spec: &Value, fallback_user: &str, fallback_groups: &[String]) -> Result<Request, String> {
    let user_name = spec.get("user").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| fallback_user.to_string());
    let user_groups = spec
        .get("groups")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_else(|| fallback_groups.to_vec());

    let resource_attrs = spec.get("resourceAttributes").filter(|v| !v.is_null());
    let non_resource_attrs = spec.get("nonResourceAttributes").filter(|v| !v.is_null());

    match (resource_attrs, non_resource_attrs) {
        (Some(ra), None) => Ok(Request {
            user_name,
            user_groups,
            is_resource: true,
            namespace: ra.get("namespace").and_then(Value::as_str).unwrap_or("").to_string(),
            verb: ra.get("verb").and_then(Value::as_str).unwrap_or("").to_string(),
            group: ra.get("group").and_then(Value::as_str).unwrap_or("").to_string(),
            resource: ra.get("resource").and_then(Value::as_str).unwrap_or("").to_string(),
            subresource: ra.get("subresource").and_then(Value::as_str).unwrap_or("").to_string(),
            name: ra.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            path: String::new(),
        }),
        (None, Some(nra)) => Ok(Request {
            user_name,
            user_groups,
            is_resource: false,
            namespace: String::new(),
            verb: nra.get("verb").and_then(Value::as_str).unwrap_or("").to_string(),
            group: String::new(),
            resource: String::new(),
            subresource: String::new(),
            name: String::new(),
            path: nra.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
        }),
        (None, None) => Err("either resourceAttributes or nonResourceAttributes must be specified".to_string()),
        (Some(_), Some(_)) => Err("resourceAttributes and nonResourceAttributes are mutually exclusive".to_string()),
    }
}

/// Real upstream's own `SubjectAccessReviewStatus` shape, populated with
/// only what this crate's RBAC engine can honestly answer — see this
/// module's own doc comment for what's deliberately omitted.
pub fn build_status(allowed: bool) -> Value {
    serde_json::json!({"allowed": allowed})
}

/// `SelfSubjectRulesReview`'s own real `SubjectRulesReviewStatus` shape:
/// every `PolicyRule` a subject's own already-resolved rule set
/// (`authz::resolve::rules_for`'s own output) carries, split into real
/// upstream's `ResourceRule`/`NonResourceRule` — a rule contributes a
/// `ResourceRule` entry when it names any `resources` and/or a
/// `NonResourceRule` entry when it names any `nonResourceURLs` (a rule
/// naming both, unusual but not invalid, contributes to both lists,
/// matching real upstream's own `RulesFor` conversion, which makes the
/// same per-field decision rather than treating a rule as one kind or
/// the other). `incomplete`/`evaluationError` are set from
/// `resolve::Resolved::errors` — a non-empty error list means real
/// upstream's own "the rules found are correct, but the list may not be
/// complete" caveat applies here too.
pub fn build_rules_status(rules: &[crate::authz::rbac::PolicyRule], errors: &[String]) -> Value {
    let mut resource_rules = Vec::new();
    let mut non_resource_rules = Vec::new();
    for rule in rules {
        if !rule.resources.is_empty() {
            resource_rules.push(serde_json::json!({
                "verbs": rule.verbs,
                "apiGroups": rule.api_groups,
                "resources": rule.resources,
                "resourceNames": rule.resource_names,
            }));
        }
        if !rule.non_resource_urls.is_empty() {
            non_resource_rules.push(serde_json::json!({"verbs": rule.verbs, "nonResourceURLs": rule.non_resource_urls}));
        }
    }
    let mut status = serde_json::json!({"resourceRules": resource_rules, "nonResourceRules": non_resource_rules, "incomplete": !errors.is_empty()});
    if !errors.is_empty() {
        status["evaluationError"] = serde_json::json!(errors.join("; "));
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_resource_attributes_with_its_own_user_and_groups() {
        let spec = json!({
            "user": "alice",
            "groups": ["devs"],
            "resourceAttributes": {"namespace": "default", "verb": "get", "resource": "pods"},
        });
        let req = parse_spec(&spec, "fallback", &["fallback-group".to_string()]).unwrap();
        assert_eq!(req.user_name, "alice");
        assert_eq!(req.user_groups, vec!["devs".to_string()]);
        assert!(req.is_resource);
        assert_eq!(req.namespace, "default");
        assert_eq!(req.verb, "get");
        assert_eq!(req.resource, "pods");
    }

    #[test]
    fn falls_back_to_the_caller_identity_when_the_spec_carries_none() {
        // The SelfSubjectAccessReview case -- its real schema has no
        // user/groups fields at all.
        let spec = json!({"resourceAttributes": {"verb": "list", "resource": "secrets"}});
        let req = parse_spec(&spec, "system:serviceaccount:default:sa1", &["system:authenticated".to_string()]).unwrap();
        assert_eq!(req.user_name, "system:serviceaccount:default:sa1");
        assert_eq!(req.user_groups, vec!["system:authenticated".to_string()]);
    }

    #[test]
    fn parses_non_resource_attributes() {
        let spec = json!({"nonResourceAttributes": {"path": "/healthz", "verb": "get"}});
        let req = parse_spec(&spec, "alice", &[]).unwrap();
        assert!(!req.is_resource);
        assert_eq!(req.path, "/healthz");
        assert_eq!(req.verb, "get");
    }

    #[test]
    fn rejects_neither_attributes_kind() {
        assert!(parse_spec(&json!({}), "alice", &[]).is_err());
    }

    #[test]
    fn rejects_both_attributes_kinds_at_once() {
        let spec = json!({
            "resourceAttributes": {"verb": "get", "resource": "pods"},
            "nonResourceAttributes": {"path": "/healthz", "verb": "get"},
        });
        assert!(parse_spec(&spec, "alice", &[]).is_err());
    }

    #[test]
    fn build_status_shape() {
        assert_eq!(build_status(true), json!({"allowed": true}));
        assert_eq!(build_status(false), json!({"allowed": false}));
    }

    #[test]
    fn build_rules_status_splits_resource_and_non_resource_rules() {
        use crate::authz::rbac::PolicyRule;
        let rules = vec![
            PolicyRule { verbs: vec!["get".to_string()], api_groups: vec!["".to_string()], resources: vec!["pods".to_string()], resource_names: vec![], non_resource_urls: vec![] },
            PolicyRule { verbs: vec!["get".to_string()], api_groups: vec![], resources: vec![], resource_names: vec![], non_resource_urls: vec!["/healthz".to_string()] },
        ];
        let status = build_rules_status(&rules, &[]);
        assert_eq!(status["resourceRules"].as_array().unwrap().len(), 1);
        assert_eq!(status["nonResourceRules"].as_array().unwrap().len(), 1);
        assert_eq!(status["incomplete"], json!(false));
        assert!(status.get("evaluationError").is_none());
    }

    #[test]
    fn build_rules_status_marks_incomplete_on_a_resolution_error() {
        let status = build_rules_status(&[], &["failed to resolve RoleBinding x".to_string()]);
        assert_eq!(status["incomplete"], json!(true));
        assert_eq!(status["evaluationError"], json!("failed to resolve RoleBinding x"));
    }
}
