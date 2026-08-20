//! Group J: admission control. Real upstream chains many built-in plugins
//! (`NamespaceLifecycle`, `LimitRanger`, `ServiceAccount`, `ResourceQuota`,
//! ...) plus mutating/validating webhooks and
//! ValidatingAdmissionPolicy/MutatingAdmissionPolicy ahead of every
//! write — none of that chaining machinery exists yet, only the first
//! plugin, landed and wired directly (no generic `Interface`/registry
//! abstraction to hang a second plugin off of yet — see this module's own
//! "not yet landed" note below for why that's a deliberate order, not an
//! oversight).
//!
//! `attributes` — the minimal `(operation, group, resource, namespace,
//! name)` tuple a plugin decides against, a real-upstream-`Attributes`
//! subset (see that module's own doc comment for why the rest isn't
//! modeled yet).
//! `namespace_lifecycle` — `NamespaceLifecycle`, a faithful port of real
//! upstream's own plugin (see that module's own doc comment for the exact
//! rules and what's honestly simplified: no informer-cache staleness
//! workarounds, since this crate has no cache in the admission path to be
//! stale). Validating only.
//! `default_toleration_seconds` — `DefaultTolerationSeconds`, this crate's
//! first **mutating** plugin: appends default `NoExecute` tolerations for
//! the `node.kubernetes.io/not-ready`/`unreachable` taints to a `Pod` that
//! doesn't already have its own (see that module's own doc comment for the
//! exact matching rule, ported from upstream's real loop). A pure
//! `Value -> Value` transform, no I/O needed — unlike
//! `namespace_lifecycle`, nothing about this decision depends on other
//! cluster state.
//! Both are **wired into `server::listener`, unconditionally** — neither
//! plugin needs operator-provisioned bootstrap data (unlike Group I's
//! RBAC), so there's no "could lock every request out" risk to gate
//! behind a config flag.
//!
//! Status: started (see docs/APISERVER.md). **Not yet landed**: every
//! other built-in plugin (`LimitRanger`, `ServiceAccount`,
//! `ResourceQuota`, `PodSecurity`, ...), a generic plugin-chain/registry
//! abstraction to run more than one plugin without `server::listener`
//! hand-calling each by name, mutating/validating admission webhooks, and
//! ValidatingAdmissionPolicy/MutatingAdmissionPolicy (CEL-based — this
//! crate already has `cel_ext` for a different purpose, not yet reused
//! here).

pub mod attributes;
pub mod default_toleration_seconds;
pub mod namespace_lifecycle;
