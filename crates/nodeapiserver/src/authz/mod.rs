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
//! exact shape and scope. **Wired into `server::listener`** as its own
//! `POST` branch (both review kinds, unconditionally — this doesn't
//! gate anything itself, it just answers "would RBAC allow this",
//! independent of whether `enforce_rbac` is even on).
//!
//! Status: in progress (see docs/APISERVER.md). The Node authorizer and
//! webhook authorization are not started;
//! `localsubjectaccessreviews`/`selfsubjectrulesreviews` aren't wired yet
//! (named follow-ups, same primitives).

pub mod rbac;
pub mod resolve;
pub mod sar;
pub mod subject;
