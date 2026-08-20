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
//! `service_account` — `ServiceAccount`, both mutating and validating,
//! `CREATE`-only: defaults `spec.serviceAccountName`, requires the
//! referenced `ServiceAccount` to exist, auto-mounts its token volume
//! unless opted out, copies its `imagePullSecrets`, and validates a
//! mirror pod's three real restrictions. See that module's own doc
//! comment for the two real pieces of upstream's plugin this one doesn't
//! port (`LimitSecretReferences`, off by default upstream anyway, and the
//! `ephemeralcontainers` subresource path, which this crate doesn't serve
//! at all yet).
//!
//! `default_storage_class` — `DefaultStorageClass`, mutating, `CREATE`-only:
//! a `PersistentVolumeClaim` with no class of its own gets
//! `spec.storageClassName` set to whichever `StorageClass` is marked
//! default (real upstream's own default-selection + tie-break rules,
//! ported exactly — see that module's own doc comment).
//!
//! `limit_ranger` — `LimitRanger`, mutating (pods, `CREATE` only) +
//! validating (pods and `PersistentVolumeClaim`s): defaults a container's
//! missing requests/limits from its namespace's `LimitRange` objects and
//! enforces their min/max/ratio constraints. Container-level and
//! PVC-level only for now — pod-level (`LimitTypePod`) aggregate
//! enforcement is a named, separate not-yet-ported gap (see that module's
//! own doc comment). Built on [`crate::scheme::quantity::Quantity`] for
//! real min/max/ratio comparisons.
//!
//! `pod_security` — `PodSecurity`, validating, `CREATE`-only: enforces
//! whichever Pod Security Standards level a namespace's
//! `pod-security.kubernetes.io/enforce` label requests
//! (`baseline`/`restricted`). **All eighteen real checks are ported** —
//! all twelve `baseline`-level and all six `restricted`-level, including
//! real upstream's own `OverrideCheckIDs` (the three baseline checks a
//! stronger restricted-level check strictly supersedes are suppressed at
//! `Restricted`, not double-reported) — see that module's own doc
//! comment for exactly which upstream variant of each check.
//!
//! All six plugins are **wired into `server::listener`, unconditionally**
//! — none needs operator-provisioned bootstrap data (unlike Group I's
//! RBAC), so there's no "could lock every request out" risk to gate
//! behind a config flag.
//!
//! Status: started (see docs/APISERVER.md). **Not yet landed**: every
//! other built-in plugin (`ResourceQuota`, ...), pod-level `LimitRange`
//! enforcement, a generic plugin-chain/registry abstraction to run more
//! than one plugin without `server::listener` hand-calling each by name,
//! mutating/validating admission webhooks, and ValidatingAdmissionPolicy/
//! MutatingAdmissionPolicy (CEL-based — this crate already has `cel_ext`
//! for a different purpose, not yet reused here).

pub mod attributes;
pub mod default_storage_class;
pub mod default_toleration_seconds;
pub mod limit_ranger;
pub mod namespace_lifecycle;
pub mod pod_security;
pub mod service_account;
