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
//! **Wired into `server::listener`**, opt-in — see `authz`'s own module
//! doc comment for the exact gate (`config::Config::enforce_rbac`) and
//! which verbs it covers.

use crate::authz::rbac::PolicyRule;
use crate::authz::subject::{first_applicable_subject, Subject, SubjectKind};
use crate::server::rest::{self, GetOutcome, ListOutcome};
use crate::storage::client::StorageClient;
use serde_json::Value;

const GROUP: &str = "rbac.authorization.k8s.io";
const VERSION: &str = "v1";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub rules: Vec<PolicyRule>,
    /// Non-fatal — every rule that *could* be resolved is still in
    /// `rules`, matching upstream's own additive posture (see this
    /// module's own doc comment).
    pub errors: Vec<String>,
}

/// A request-local, read-only snapshot of the RBAC objects needed by CEL's
/// `authorizer` library. CEL calls are synchronous, so the admission path
/// loads this snapshot before evaluation and the check functions resolve
/// subjects and roles from it without doing I/O in the interpreter.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Snapshot {
    cluster_role_bindings: Vec<Value>,
    cluster_roles: Vec<Value>,
    role_bindings: Vec<Value>,
    roles: Vec<Value>,
}

/// Load the RBAC objects used by [`Snapshot::rules_for`]. A failure to list
/// any part of the snapshot is returned to the caller instead of silently
/// turning an authorization error into an allow.
pub async fn load_snapshot(storage: &mut StorageClient) -> Result<Snapshot, String> {
    Ok(Snapshot {
        cluster_role_bindings: snapshot_list(storage, "clusterrolebindings", None).await?,
        cluster_roles: snapshot_list(storage, "clusterroles", None).await?,
        role_bindings: snapshot_list(storage, "rolebindings", None).await?,
        roles: snapshot_list(storage, "roles", None).await?,
    })
}

async fn snapshot_list(storage: &mut StorageClient, resource: &str, namespace: Option<&str>) -> Result<Vec<Value>, String> {
    match rest::list(storage, None, GROUP, VERSION, resource, namespace, "", "", 0, "").await {
        Ok(ListOutcome::Found(list)) => Ok(list.get("items").and_then(Value::as_array).cloned().unwrap_or_default()),
        Ok(ListOutcome::UnknownResource) | Ok(ListOutcome::InvalidContinueToken) => Err(format!("{resource} is unknown to this build")),
        Err(error) => Err(format!("listing {resource}: {error}")),
    }
}

impl Snapshot {
    /// Resolve all rules applying to a principal in a namespace using only
    /// this snapshot. The matching and role-reference rules are the same as
    /// [`rules_for`], but no network or storage calls are possible here.
    pub fn rules_for(&self, user_name: &str, user_groups: &[String], namespace: &str) -> Resolved {
        let mut resolved = Resolved::default();
        for binding in &self.cluster_role_bindings {
            accumulate_snapshot_binding(self, binding, user_name, user_groups, "", &mut resolved);
        }
        if !namespace.is_empty() {
            for binding in &self.role_bindings {
                if binding_namespace(binding) == namespace {
                    accumulate_snapshot_binding(self, binding, user_name, user_groups, namespace, &mut resolved);
                }
            }
        }
        resolved
    }
}

fn binding_namespace(binding: &Value) -> &str {
    binding.get("metadata").and_then(|metadata| metadata.get("namespace")).and_then(Value::as_str).unwrap_or("")
}

fn accumulate_snapshot_binding(snapshot: &Snapshot, binding: &Value, user_name: &str, user_groups: &[String], binding_ns: &str, resolved: &mut Resolved) {
    let subjects = parse_subjects(binding);
    if first_applicable_subject(user_name, user_groups, &subjects, binding_ns).is_none() {
        return;
    }

    let Some(role_ref) = binding.get("roleRef") else {
        resolved.errors.push("binding has no roleRef".to_string());
        return;
    };
    let kind = role_ref.get("kind").and_then(Value::as_str).unwrap_or("");
    let name = role_ref.get("name").and_then(Value::as_str).unwrap_or("");
    let role = match kind {
        "Role" => snapshot
            .roles
            .iter()
            .find(|role| binding_namespace(role) == binding_ns && object_name(role) == name),
        "ClusterRole" => snapshot.cluster_roles.iter().find(|role| object_name(role) == name),
        other => {
            resolved.errors.push(format!("unsupported roleRef kind {other:?}"));
            return;
        }
    };
    match role {
        Some(role) => resolved.rules.extend(parse_policy_rules(role)),
        None => resolved.errors.push(format!("{kind} {name:?} referenced by a binding was not found")),
    }
}

fn object_name(object: &Value) -> &str {
    object.get("metadata").and_then(|metadata| metadata.get("name")).and_then(Value::as_str).unwrap_or("")
}

/// `VisitRulesFor`: every `PolicyRule` that applies to `user_name`/
/// `user_groups` in `namespace` (`""` for none — only `ClusterRoleBinding`s
/// are consulted then, matching upstream's own `if len(namespace) > 0`
/// guard around the `RoleBinding` half).
///
/// `cache_registry`, when given, is consulted for the `clusterrolebindings`/
/// `rolebindings` watch caches, *and* (threaded down into
/// [`accumulate_binding`]) the `clusterroles`/`roles` caches each binding
/// resolves against — so this, called on every authorized request via
/// `authz::request_allowed`, reads nodeapiserver's own in-process cache
/// instead of paying a real nodestore round trip on every single request.
/// `None` (the admission-time `signer_request_allowed` caller, which has
/// no registry handle in scope) falls back to the uncached path exactly
/// as before; `rest::list`/`rest::get` themselves already tolerate
/// `cache: None` or a cache that hasn't finished its first sync.
pub async fn rules_for(storage: &mut StorageClient, user_name: &str, user_groups: &[String], namespace: &str, cache_registry: Option<&crate::cacher::CacheRegistry>) -> Resolved {
    let mut resolved = Resolved::default();
    let crb_cache = cache_registry.and_then(|registry| registry.get(GROUP, VERSION, "clusterrolebindings"));

    match rest::list(storage, crb_cache.as_ref(), GROUP, VERSION, "clusterrolebindings", None, "", "", 0, "").await {
        Ok(ListOutcome::Found(list)) => {
            for item in list["items"].as_array().cloned().unwrap_or_default() {
                accumulate_binding(storage, cache_registry, &item, user_name, user_groups, "", &mut resolved).await;
            }
        }
        Ok(ListOutcome::UnknownResource) | Ok(ListOutcome::InvalidContinueToken) => resolved.errors.push("clusterrolebindings is unknown to this build".to_string()),
        Err(e) => resolved.errors.push(format!("listing clusterrolebindings: {e}")),
    }

    if !namespace.is_empty() {
        let rb_cache = cache_registry.and_then(|registry| registry.get(GROUP, VERSION, "rolebindings"));
        match rest::list(storage, rb_cache.as_ref(), GROUP, VERSION, "rolebindings", Some(namespace), "", "", 0, "").await {
            Ok(ListOutcome::Found(list)) => {
                for item in list["items"].as_array().cloned().unwrap_or_default() {
                    accumulate_binding(storage, cache_registry, &item, user_name, user_groups, namespace, &mut resolved).await;
                }
            }
            Ok(ListOutcome::UnknownResource) | Ok(ListOutcome::InvalidContinueToken) => resolved.errors.push("rolebindings is unknown to this build".to_string()),
            Err(e) => resolved.errors.push(format!("listing rolebindings: {e}")),
        }
    }

    resolved
}

/// One binding: skip it if none of its `Subjects` apply to this user,
/// else resolve its `RoleRef` (`Role`, only meaningful with a real
/// `binding_namespace`, or `ClusterRole`) and extend `resolved.rules`
/// with the referenced role's own `Rules`.
///
/// `cache_registry`, threaded down from [`rules_for`], is consulted here
/// too (#562): the bindings list above already reads from the watch
/// cache, but each binding's *referenced* Role/ClusterRole was still an
/// uncached `rest::get` -- one uncached Range call to nodestore per
/// binding, on every single authorized request. Found live: nodestore's
/// Range-call tally showed `clusterroles` dominating call volume by a
/// wide margin over every other resource, including the cached
/// `clusterrolebindings` themselves.
async fn accumulate_binding(storage: &mut StorageClient, cache_registry: Option<&crate::cacher::CacheRegistry>, binding: &Value, user_name: &str, user_groups: &[String], binding_namespace: &str, resolved: &mut Resolved) {
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
        "Role" => {
            let cache = cache_registry.and_then(|registry| registry.get(GROUP, VERSION, "roles"));
            rest::get(storage, cache.as_ref(), GROUP, VERSION, "roles", Some(binding_namespace), name).await
        }
        "ClusterRole" => {
            let cache = cache_registry.and_then(|registry| registry.get(GROUP, VERSION, "clusterroles"));
            rest::get(storage, cache.as_ref(), GROUP, VERSION, "clusterroles", None, name).await
        }
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

    #[test]
    fn a_snapshot_resolves_namespaced_roles_without_storage_calls() {
        let snapshot = Snapshot {
            cluster_role_bindings: vec![json!({
                "subjects": [{"kind": "User", "name": "alice"}],
                "roleRef": {"kind": "ClusterRole", "name": "reader"},
            })],
            cluster_roles: vec![json!({
                "metadata": {"name": "reader"},
                "rules": [{"verbs": ["get"], "apiGroups": ["apps"], "resources": ["deployments"]}],
            })],
            role_bindings: vec![json!({
                "metadata": {"namespace": "team-a"},
                "subjects": [{"kind": "User", "name": "alice"}],
                "roleRef": {"kind": "Role", "name": "writer"},
            })],
            roles: vec![json!({
                "metadata": {"namespace": "team-a", "name": "writer"},
                "rules": [{"verbs": ["update"], "apiGroups": ["apps"], "resources": ["deployments"]}],
            })],
        };

        let resolved = snapshot.rules_for("alice", &[], "team-a");
        assert_eq!(resolved.errors, Vec::<String>::new());
        assert_eq!(resolved.rules.len(), 2);
        assert!(crate::authz::rbac::rules_allow(
            &crate::authz::rbac::RequestAttributes { is_resource_request: true, verb: "update", api_group: "apps", resource: "deployments", subresource: "", name: "", path: "" },
            &resolved.rules,
        ));
    }
}
