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
//! calls `resolve::rules_for` + `rbac::rules_allow` to gate `GET`/`LIST`
//! with a real `403` on denial — but only when
//! `NODEAPISERVER_ENFORCE_RBAC` (`config::Config::enforce_rbac`) is set,
//! off by default. That field's own doc comment explains why: enabling
//! deny-by-default RBAC before Group O's bootstrap `ClusterRole`/
//! `ClusterRoleBinding` set exists can lock every request out with no
//! path back in. A request with no established x509 identity is
//! evaluated as the real anonymous user/group upstream itself uses
//! (`system:anonymous`/`system:unauthenticated`), not silently exempted.
//!
//! Status: in progress (see docs/APISERVER.md). The Node authorizer,
//! webhook authorization, and SubjectAccessReview/SelfSubjectAccessReview
//! are not started.

pub mod rbac;
pub mod resolve;
pub mod subject;
