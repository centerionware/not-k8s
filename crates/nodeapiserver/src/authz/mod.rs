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
//! **Both are pure evaluation primitives only — not yet wired to real
//! `Role`/`RoleBinding`/`ClusterRole`/`ClusterRoleBinding` objects**:
//! resolving which bindings/rules apply to a given subject in a given
//! namespace needs those objects fetched from storage (real upstream's
//! `DefaultRuleResolver`, which combines exactly these two primitives
//! over real listed/fetched objects), which isn't built yet. Not wired
//! into `server::listener` either: every request is still served
//! regardless of the identity `authn::x509` may have established for it
//! (that module's own doc comment names this same gap from the
//! authentication side).
//!
//! Status: in progress (see docs/APISERVER.md). The Node authorizer,
//! webhook authorization, and SubjectAccessReview/SelfSubjectAccessReview
//! are not started.

pub mod rbac;
pub mod subject;
