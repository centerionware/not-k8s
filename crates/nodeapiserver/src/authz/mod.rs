//! Group I: authorization. RBAC, the Node authorizer, webhook
//! authorization, SubjectAccessReview/SelfSubjectAccessReview.
//!
//! `rbac` — the RBAC rule-matching primitive: given a `PolicyRule` and a
//! request's attributes, decide whether that one rule permits it. A
//! faithful port of real upstream's own matching functions
//! (`pkg/apis/rbac/v1/evaluation_helpers.go`'s `VerbMatches`/
//! `APIGroupMatches`/`ResourceMatches`/`ResourceNameMatches`/
//! `NonResourceURLMatches`, composed by
//! `plugin/pkg/auth/authorizer/rbac/rbac.go`'s `RuleAllows`/`RulesAllow`).
//! `subject` — does a binding's `Subjects` list include a given
//! authenticated user? A faithful port of
//! `pkg/registry/rbac/validation/rule.go`'s `appliesTo`/`appliesToUser`,
//! including the `ServiceAccount` `system:serviceaccount:<ns>:<name>`
//! username convention.
//!
//! `resolve` — the storage-backed half: `rules_for(storage, user_name,
//! user_groups, namespace)` lists real `ClusterRoleBinding`/`RoleBinding`
//! objects (via `server::rest::list`, no per-type Go code), keeps the
//! ones whose subjects apply (`subject`), and resolves each one's
//! `RoleRef` to a real `Role`/`ClusterRole`'s rules (via
//! `server::rest::get`) — real upstream's own `DefaultRuleResolver`,
//! ported. Errors resolving one binding are collected, not fatal
//! (matching upstream's own "policy rules are purely additive" posture).
//!
//! **Wired into `server::listener`, opt-in**: `server::listener::handle`
//! calls `resolve::rules_for` + `rbac::rules_allow` to gate
//! `GET`/`LIST`/`CREATE`/`DELETE`/`UPDATE` with a real `403` on denial —
//! but only when
//! `NODEAPISERVER_ENFORCE_RBAC` (`config::Config::enforce_rbac`) is set,
//! off by default. That field's own doc comment explains why: enabling
//! deny-by-default RBAC before Group O's bootstrap `ClusterRole`/
//! `ClusterRoleBinding` set exists can lock every request out with no
//! path back in. A request with no established x509 identity is
//! evaluated as the real anonymous user/group upstream itself uses
//! (`system:anonymous`/`system:unauthenticated`), not silently exempted.
//!
//! `sar` — `SubjectAccessReview`/`SelfSubjectAccessReview`, a thin
//! wiring of `resolve`/`rbac` to real upstream's own virtual (never
//! persisted) review API — see that module's own doc comment for the
//! exact shape and scope. **Wired into `server::listener`**, unconditionally
//! (this doesn't gate anything itself, it just answers requests against
//! the RBAC engine's own state, independent of whether `enforce_rbac` is
//! even on): `SubjectAccessReview`/`SelfSubjectAccessReview`/
//! `LocalSubjectAccessReview` share one `POST` branch,
//! `SelfSubjectRulesReview` has its own (different response shape).
//!
//! `webhook` — an optional, fail-closed HTTP `SubjectAccessReview` client
//! for delegating authorization decisions to an external authorizer.
//!
//! Status: in progress (see docs/APISERVER.md). The Node authorizer is wired
//! separately; webhook authorization is enabled only when its endpoint is
//! configured.

//! `node` — the request-specific Node authorizer, evaluated before RBAC for
//! node identities, including storage-backed relationship checks for
//! node-owned resources.

pub mod node;
pub mod rbac;
pub mod resolve;
pub mod sar;
pub mod subject;
pub mod webhook;

use crate::authn::x509::Identity;
use crate::server::path::RequestInfo;
use crate::storage::client::StorageClient;

/// Runs the authorization chain used by the nodeapiserver target: the
/// request-specific Node authorizer first, then storage-backed RBAC when
/// the Node authorizer has no opinion. This is the same ordering as
/// kube-apiserver's `Node,RBAC` mode.
pub async fn request_allowed(
    storage: &mut StorageClient,
    identity: Option<&Identity>,
    info: &RequestInfo,
    cache_registry: Option<&crate::cacher::CacheRegistry>,
) -> Result<bool, String> {
    match node::authorize(storage, identity, info, cache_registry).await? {
        node::Decision::Allow => return Ok(true),
        node::Decision::Deny => return Ok(false),
        node::Decision::NoOpinion => {}
    }

    let (user_name, user_groups): (&str, Vec<String>) = match identity {
        Some(identity) => (identity.name.as_str(), identity.groups.clone()),
        None => (
            "system:anonymous",
            vec!["system:unauthenticated".to_string()],
        ),
    };
    let resolved = resolve::rules_for(storage, user_name, &user_groups, &info.namespace, cache_registry).await;
    let attrs = rbac::RequestAttributes {
        is_resource_request: info.is_resource_request,
        verb: &info.verb,
        api_group: &info.api_group,
        resource: &info.resource,
        subresource: &info.subresource,
        name: &info.name,
        path: &info.path,
    };
    Ok(rbac::rules_allow(&attrs, &resolved.rules))
}
