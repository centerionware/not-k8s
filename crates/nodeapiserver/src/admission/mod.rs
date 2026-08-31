//! Group J: admission control. Real upstream chains many built-in plugins
//! (`NamespaceLifecycle`, `LimitRanger`, `ServiceAccount`, `ResourceQuota`,
//! `PodSecurity`, ...) plus mutating/validating webhooks and
//! ValidatingAdmissionPolicy/MutatingAdmissionPolicy ahead of every
//! write. **Eight built-in plugins are now landed and wired** (listed
//! below). Pure mutators run through [`chain::MutatingRegistry`]; the
//! storage-backed mutators and validators still need a broader async
//! registry because their I/O and failure-policy behavior is request-specific.
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
//! `CREATE`-only for ordinary Pods: defaults `spec.serviceAccountName`,
//! requires the referenced `ServiceAccount` to exist, auto-mounts its token
//! volume unless opted out, copies its `imagePullSecrets`, and validates a
//! mirror pod's three real restrictions and the opt-in
//! `enforce-mountable-secrets` check. Its separate
//! `ephemeralcontainers` validation path also checks new ephemeral
//! containers against the referenced account's allowed Secret references.
//! `node_restriction` — `NodeRestriction`, the body-sensitive half of the
//! Node authorizer chain: restricts node-owned objects to the requesting node
//! and validates mirror-Pod ownership, node labels, token bindings, leases,
//! CSINodes, and ResourceSlices.
//!
//! `default_storage_class` — `DefaultStorageClass`, mutating, `CREATE`-only:
//! a `PersistentVolumeClaim` with no class of its own gets
//! `spec.storageClassName` set to whichever `StorageClass` is marked
//! default (real upstream's own default-selection + tie-break rules,
//! ported exactly — see that module's own doc comment).
//!
//! `default_ingress_class` — `DefaultIngressClass`, mutating, `CREATE`-only:
//! an Ingress without an explicit class or legacy class annotation gets the
//! newest default `IngressClass`, matching upstream's lister and tie-break.
//!
//! `storage_object_in_use_protection` — `StorageObjectInUseProtection`,
//! mutating, `CREATE`-only: adds the standard PV, PVC, and
//! VolumeAttributesClass protection finalizers that the nodecontroller
//! protection controllers later remove when the object is safe to delete.
//!
//! `runtime_class` — `RuntimeClass`, mutating and validating, `CREATE`-only
//! for ordinary Pods: resolves the named cluster-scoped RuntimeClass, applies
//! its overhead and scheduling constraints, rejects selector conflicts, and
//! rejects Pod overhead without a matching RuntimeClass definition.
//!
//! `priority` — `Priority`, mutating Pod `CREATE`/`UPDATE` and validating
//! `PriorityClass` `CREATE`/`UPDATE`: resolves a named or global-default
//! PriorityClass, sets Pod priority and preemption policy, preserves those
//! fields across updates, and prevents competing global defaults.
//!
//! `pvc_resize` — `PersistentVolumeClaimResize`, validating, `UPDATE`-only:
//! allows a PVC expansion only when it is Bound, retains the same class, and
//! that StorageClass explicitly permits volume expansion.
//!
//! `limit_ranger` — `LimitRanger`, mutating (pods, `CREATE` only) +
//! validating (pods and `PersistentVolumeClaim`s): defaults a container's
//! missing requests/limits from its namespace's `LimitRange` objects and
//! enforces their min/max/ratio constraints — container-level, pod-level
//! (real upstream's own aggregated-total/sidecar-accounting rules ported
//! exactly, see that module's own doc comment), and PVC-level. Built on
//! [`crate::scheme::quantity::Quantity`] for real min/max/ratio
//! comparisons (which also gained `+`/`max` for this).
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
//! `resource_quota` — `ResourceQuota`, validating, `CREATE`-only. Three
//! specialized evaluators — **pods, PersistentVolumeClaims, and
//! Services** — plus a fourth, **generic** one
//! (`check_object_count_create`, mirroring real upstream's own
//! `generic.objectCountEvaluator`) that covers every other resource kind
//! via the stable `count/<resource>` (core group) /
//! `count/<resource>.<group>` (other groups) quota-key convention, with
//! no scope matching (matching real upstream's own `MatchesNoScopeFunc`
//! for that evaluator). Real upstream's own registry
//! (`pkg/quota/v1/evaluator/core/registry.go`) is exactly this shape:
//! three specials, everything else generic — so this crate doesn't need
//! a real per-type evaluator for secrets/configmaps/etc. to already
//! cover them. Forbids a resource `CREATE` that would push a namespace's
//! tracked usage
//! (`pods`/`cpu`/`requests.cpu`/`limits.cpu`/`memory`/`requests.memory`/
//! `limits.memory`/`ephemeral-storage`/`requests.ephemeral-storage`/
//! `limits.ephemeral-storage`/`hugepages-<size>`/
//! `requests.hugepages-<size>`/`persistentvolumeclaims`/`requests.storage`/
//! `services`/`services.nodeports`/`services.loadbalancers`/
//! `count/<resource>[.<group>]`/extended resources in their real
//! `requests.<name>`-only form, e.g. `requests.nvidia.com/gpu` (real
//! upstream's own `isExtendedResourceNameForQuota`: overcommit isn't
//! supported for extended resources, so no bare or `limits.`-prefixed
//! form is ever recognized)/the PVC evaluator's own real per-storage-class
//! resource family (`<class>.storageclass.storage.k8s.io/
//! persistentvolumeclaims` and `.../requests.storage`)) over any
//! `ResourceQuota`'s own `spec.hard`. All six real `spec.scopes` names are matched for pods
//! (`Terminating`/`NotTerminating`/`BestEffort`/`NotBestEffort` —
//! including a real `ComputePodQOS` port —, `PriorityClass`, and
//! `CrossNamespacePodAffinity`), **and so is the richer
//! `spec.scopeSelector.matchExpressions` form** — `PriorityClass` gains
//! its real `In`/`NotIn`/`Exists`/`DoesNotExist` operators against a
//! specific set of priority class names, while every other scope name
//! ignores the operator/values the same way real upstream's own
//! `podMatchesScopeFunc` switch does; PVCs, services, and the generic
//! evaluator only match an *unscoped* quota (matching each
//! evaluator's own real behavior). `status.used` is persisted after an
//! admitted create, while admission-time usage is still recomputed live
//! from a fresh object list (see that module's own doc comment for the one
//! real concurrency-race consequence this carries). Reuses `limit_ranger`'s own
//! `pod_requests`/`pod_limits` for the aggregation, since real upstream's
//! quota usage function calls the exact same underlying helper.
//!
//! All eight plugins are **wired into `server::listener`, unconditionally**
//! — pure mutators are invoked through `chain::MutatingRegistry`, while
//! storage-backed stages remain in the listener until their async registry
//! adapter lands. None needs operator-provisioned bootstrap data (unlike
//! Group I's RBAC), so there's no "could lock every request out" risk to
//! gate behind a config flag.
//!
//! `match_conditions` — real upstream's own `matchconditions.Matcher`
//! (the real CEL-based pre-filter shared by both mutating/validating
//! admission webhooks' own `spec.matchConditions` and
//! `ValidatingAdmissionPolicy`'s own `spec.matchConditions`), landed as
//! a pure, standalone primitive consumed by the storage-backed policy
//! enforcement adapter and the webhook dispatcher.
//!
//! `policy_matching` — the other half of "does this policy even apply to
//! this request": real upstream's own `resourceRules`/
//! `excludeResourceRules` matching (`rules.Matcher`), `namespaceSelector`/
//! `objectSelector` matching (reusing `cacher::selector`'s own label
//! matcher), `request` CEL variable construction
//! (`CreateAdmissionRequest`), and the real `object`/`oldObject`/
//! `request`/`params` variable set assembly (`build_eval_vars`) and
//! declaration-order `variables` composition (`compose_variables`) —
//! `object`/`oldObject`/`params` bind to a real CEL `null`, not an absent
//! variable, when the caller has none. See that module's own doc comment
//! for the named, honest gap that the pure wrapper remains scope-agnostic
//! without discovery context. The storage-backed policy adapters populate
//! `kind`/`userInfo` and consume these pure matching and variable-building
//! primitives for real policy requests.
//!
//! `policy_validations` — the actual `spec.validations[]` decision: real
//! upstream's own `validator.Validate`, given an already-bound variable
//! set, evaluates every validation (no short-circuit, unlike
//! `match_conditions`) and produces a real `Admit`/`Deny` decision per
//! rule with the same message-resolution order real upstream uses
//! (`messageExpression` if valid, else the rule's own `message`, else a
//! generic `"failed expression: ..."`) and the same `failurePolicy`-
//! governed handling of a compile/evaluation error. See that module's
//! own doc comment for the exact real scope and the two new
//! `cel_ext::eval_string_with_vars`/`eval_string_with_vars_and_deadline`
//! primitives it's built on (`messageExpression` is CEL evaluating to a
//! `string`, not a `bool` — the first real use of a non-boolean CEL
//! result in this crate).
//!
//! `validating_admission_policy` — the single real per-policy decision
//! that composes the three primitives above in real upstream's own real
//! order (`spec.matchConstraints` → `spec.matchConditions` →
//! `spec.validations`, each stage only narrowing further): given a
//! [`validating_admission_policy::PolicyDefinition`]'s borrowed view of
//! one policy's own fields and an already-bound variable set, returns the
//! real per-policy outcome (`NotApplicable`, a real `matchConditions`
//! evaluation error, or the per-rule `Decided` result). Still no I/O of
//! its own — same standalone-primitive posture as everything else in
//! this arc. `PolicyOutcome::is_denial`/`denial_message` and the
//! standalone `validation_actions_deny`/`validation_actions_warn`/
//! `validation_actions_audit` primitives round out the decision side: the
//! real "should a caller reject, warn, or audit this request" question,
//! gated on a `ValidatingAdmissionPolicyBinding`'s own
//! `validationActions`.
//!
//! `policy_decode` — decodes a real `ValidatingAdmissionPolicy` object's
//! own `spec` (wire JSON, field names verified against the vendored
//! OpenAPI schema) into `validating_admission_policy::PolicyDefinition`'s
//! own borrowed view. `DecodedPolicy::decode` takes one real policy
//! object; `DecodedPolicy::resource_rules`/`exclude_resource_rules` hand
//! back a freshly built `Vec<policy_matching::ResourceRule>` a caller
//! binds to a local before assembling a `PolicyDefinition` from it (see
//! that module's own doc comment for why — a real self-referential-struct
//! shape no single method could return by value). The last real gap
//! before a caller has actual decoded policy data to evaluate, instead of
//! hand-built test fixtures.
//!
//! `policy_enforcement` is the storage-backed adapter for
//! `ValidatingAdmissionPolicy`/`ValidatingAdmissionPolicyBinding`: it loads
//! matching bindings, resolves named or selector-based parameter references,
//! evaluates the decoded policy against the final candidate object, and
//! returns denial, warning, and audit results for the request wrapper to
//! publish before persistence.
//!
//! Status: in progress (see docs/APISERVER.md). The built-in plugins,
//! admission webhooks, and both admission-policy enforcement paths are
//! wired into `server::listener`. The remaining work is the broader
//! upstream built-in plugin set, the remaining typed mutation result forms,
//! and interpreter-level CEL fuel accounting; the standalone matching and
//! evaluation primitives remain independently testable below.

pub mod attributes;
pub mod chain;
pub mod default_ingress_class;
pub mod default_storage_class;
pub mod default_toleration_seconds;
pub mod limit_ranger;
pub mod match_conditions;
pub mod mutating_admission_policy;
pub mod namespace_lifecycle;
pub mod node_restriction;
pub mod pod_security;
pub mod policy_decode;
pub mod policy_enforcement;
pub mod policy_matching;
pub mod policy_validations;
pub mod priority;
pub mod pvc_resize;
pub mod resource_quota;
pub mod runtime_class;
pub mod service_account;
pub mod storage_object_in_use_protection;
pub mod validating_admission_policy;
pub mod webhook;
