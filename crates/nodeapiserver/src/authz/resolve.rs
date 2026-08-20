//! Resolves the real rules a subject's request is evaluated against — the
//! storage-backed half real upstream's `DefaultRuleResolver` adds on top
//! of `rbac` (rule matching) and `subject` (subject matching), a faithful
//! port of `pkg/registry/rbac/validation/rule.go`'s `VisitRulesFor`
//! (fetched and read directly): list every `ClusterRoleBinding`, keep the
//! ones whose `Subjects` apply to this user, resolve each one's `RoleRef`
//! to a `ClusterRole`'s `Rules`; then, if a namespace was given, the same
//! over `RoleBinding`s in that namespace (whose `RoleRef` may point at a
//! same-namespace `Role` *or* a `ClusterRole` — both real, real upstream
//! allows referencing a `ClusterRole` from a `RoleBinding` to reuse its
//! rules scoped to just that one namespace).
//!
//! Uses `server::rest::get`/`list` directly (not over HTTP) — this
//! crate's only generic way to read a resource without per-type Go code,
//! the same machinery every real request handler already uses. Errors
//! resolving one binding are collected, not fatal — matching real
//! upstream's own "policy rules are purely additive" posture
//! (`AuthorizationRuleResolver.RulesFor`'s own doc comment: "If an error
//! is returned, the slice of PolicyRules may not be complete, but it
//! contains all retrievable rules... policy determinations can be made on
//! the basis of those rules that are found"): a binding this build
//! couldn't resolve contributes no rules, it doesn't block evaluating
//! every other binding.
//!
//! **Not wired into `server::listener` yet** — see `authz`'s own module
//! doc comment for what's still missing before an actual request gets
//! gated by any of this.

use crate::authz::rbac::PolicyRule;
use crate::authz::subject::{first_applicable_subject, Subject, SubjectKind};
use crate::server::rest::{self, GetOutcome, ListOutcome};
use crate::storage::client::StorageClient;
use serde_json::Value;

const GROUP: &str = "rbac.authorization.k8s.io";
const VERSION: &str = "v1";

#[derive(Debug, Default)]
pub struct Resolved {
    pub rules: Vec<PolicyRule>,
    /// Non-fatal — every rule that *could* be resolved is still in
    /// `rules`, matching upstream's own additive posture (see this
    /// module's own doc comment).
    pub errors: Vec<String>,
}

/// `VisitRulesFor`: every `PolicyRule` that applies to `user_name`/
/// `user_groups` in `namespace` (`""` for none — only `ClusterRoleBinding`s
/// are consulted then, matching upstream's own `if len(namespace) > 0`
/// guard around the `RoleBinding` half).
pub async fn rules_for(storage: &mut StorageClient, user_name: &str, user_groups: &[String], namespace: &str) -> Resolved {
    let mut resolved = Resolved::default();

    match rest::list(storage, GROUP, VERSION, "clusterrolebindings", None, "", "").await {
        Ok(ListOutcome::Found(list)) => {
            for item in list["items"].as_array().cloned().unwrap_or_default() {
                accumulate_binding(storage, &item, user_name, user_groups, "", &mut resolved).await;
            }
        }
        Ok(ListOutcome::UnknownResource) => resolved.errors.push("clusterrolebindings is unknown to this build".to_string()),
        Err(e) => resolved.errors.push(format!("listing clusterrolebindings: {e}")),
    }

    if !namespace.is_empty() {
        match rest::list(storage, GROUP, VERSION, "rolebindings", Some(namespace), "", "").await {
            Ok(ListOutcome::Found(list)) => {
                for item in list["items"].as_array().cloned().unwrap_or_default() {
                    accumulate_binding(storage, &item, user_name, user_groups, namespace, &mut resolved).await;
                }
            }
            Ok(ListOutcome::UnknownResource) => resolved.errors.push("rolebindings is unknown to this build".to_string()),
            Err(e) => resolved.errors.push(format!("listing rolebindings: {e}")),
        }
    }

    resolved
}

/// One binding: skip it if none of its `Subjects` apply to this user,
/// else resolve its `RoleRef` (`Role`, only meaningful with a real
/// `binding_namespace`, or `ClusterRole`) and extend `resolved.rules`
/// with the referenced role's own `Rules`.
async fn accumulate_binding(storage: &mut StorageClient, binding: &Value, user_name: &str, user_groups: &[String], binding_namespace: &str, resolved: &mut Resolved) {
    let subjects = parse_subjects(binding);
    if first_applicable_subject(user_name, user_groups, &subjects, binding_namespace).is_none() {
        return;
    }

    let Some(role_ref) = binding.get("roleRef") else {
        resolved.errors.push("binding has no roleRef".to_string());
        return;
    };
    let kind = role_ref.get("kind").and_then(Value::as_str).unwrap_or("");
    let name = role_ref.get("name").and_then(Value::as_str).unwrap_or("");

    let role_object = match kind {
        "Role" => rest::get(storage, GROUP, VERSION, "roles", Some(binding_namespace), name).await,
        "ClusterRole" => rest::get(storage, GROUP, VERSION, "clusterroles", None, name).await,
        other => {
            resolved.errors.push(format!("unsupported roleRef kind {other:?}"));
            return;
        }
    };
    match role_object {
        Ok(GetOutcome::Found(role)) => resolved.rules.extend(parse_policy_rules(&role)),
        Ok(GetOutcome::ObjectNotFound) => resolved.errors.push(format!("{kind} {name:?} referenced by a binding was not found")),
        Ok(GetOutcome::UnknownResource) => resolved.errors.push(format!("{kind} is unknown to this build")),
        Err(e) => resolved.errors.push(format!("resolving {kind} {name:?}: {e}")),
    }
}

/// Real vendored field names (`io.k8s.api.rbac.v1.Subject`): `kind`/
/// `name`/`namespace`/`apiGroup` — `apiGroup` isn't represented at all
/// (see `authz::subject`'s own doc comment for why matching never reads
/// it). A subject whose `kind` isn't one of the three real ones, or with
/// no `name`, is skipped rather than treated as a parse error — matching
/// upstream's own tolerance for a `Subject.Kind` it doesn't recognize
/// (`appliesToUser`'s `default: return false`).
fn parse_subjects(binding: &Value) -> Vec<Subject> {
    binding
        .get("subjects")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let kind = match s.get("kind").and_then(Value::as_str)? {
                        "User" => SubjectKind::User,
                        "Group" => SubjectKind::Group,
                        "ServiceAccount" => SubjectKind::ServiceAccount,
                        _ => return None,
                    };
                    let name = s.get("name").and_then(Value::as_str)?.to_string();
                    let namespace = s.get("namespace").and_then(Value::as_str).unwrap_or("").to_string();
                    Some(Subject { kind, name, namespace })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Real vendored field names (`io.k8s.api.rbac.v1.PolicyRule`): `verbs`/
/// `apiGroups`/`resources`/`resourceNames`/`nonResourceURLs`.
fn parse_policy_rules(role: &Value) -> Vec<PolicyRule> {
    role.get("rules")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|r| PolicyRule {
                    verbs: string_array(r, "verbs"),
                    api_groups: string_array(r, "apiGroups"),
                    resources: string_array(r, "resources"),
                    resource_names: string_array(r, "resourceNames"),
                    non_resource_urls: string_array(r, "nonResourceURLs"),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn string_array(value: &Value, key: &str) -> Vec<String> {
    value.get(key).and_then(Value::as_array).map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_subjects_reads_all_three_real_kinds() {
        let binding = json!({
            "subjects": [
                {"kind": "User", "name": "alice"},
                {"kind": "Group", "name": "system:masters"},
                {"kind": "ServiceAccount", "name": "web-sa", "namespace": "default"},
            ]
        });
        let subjects = parse_subjects(&binding);
        assert_eq!(subjects.len(), 3);
        assert_eq!(subjects[0], Subject { kind: SubjectKind::User, name: "alice".to_string(), namespace: String::new() });
        assert_eq!(subjects[2], Subject { kind: SubjectKind::ServiceAccount, name: "web-sa".to_string(), namespace: "default".to_string() });
    }

    #[test]
    fn parse_subjects_skips_an_unrecognized_kind_rather_than_erroring() {
        let binding = json!({"subjects": [{"kind": "SomeFutureKind", "name": "x"}, {"kind": "User", "name": "alice"}]});
        let subjects = parse_subjects(&binding);
        assert_eq!(subjects.len(), 1);
        assert_eq!(subjects[0].name, "alice");
    }

    #[test]
    fn parse_subjects_on_a_binding_with_no_subjects_field_is_empty() {
        assert_eq!(parse_subjects(&json!({})), vec![]);
    }

    #[test]
    fn parse_policy_rules_reads_the_real_field_names() {
        let role = json!({
            "rules": [
                {"verbs": ["get", "list"], "apiGroups": [""], "resources": ["pods"], "resourceNames": ["web-1"], "nonResourceURLs": []},
            ]
        });
        let rules = parse_policy_rules(&role);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].verbs, vec!["get".to_string(), "list".to_string()]);
        assert_eq!(rules[0].api_groups, vec!["".to_string()]);
        assert_eq!(rules[0].resources, vec!["pods".to_string()]);
        assert_eq!(rules[0].resource_names, vec!["web-1".to_string()]);
    }

    #[test]
    fn parse_policy_rules_on_a_role_with_no_rules_field_is_empty() {
        assert_eq!(parse_policy_rules(&json!({})), vec![]);
    }
}
