//! `ResourceQuota` — a faithful-but-substantially-scoped port of real
//! upstream's own admission plugin
//! (`staging/src/k8s.io/apiserver/pkg/admission/plugin/resourcequota/controller.go`
//! + `pkg/quota/v1/evaluator/core/{pods,persistent_volume_claims,services}.go`,
//! release-1.34, fetched and read directly): forbids a `Pod`/
//! `PersistentVolumeClaim`/`Service` `CREATE` that would push its
//! namespace's tracked resource usage over any `ResourceQuota` object's
//! own `spec.hard` limit.
//!
//! **Three specialized evaluators, plus the generic one** — real
//! upstream's own three "special" evaluators (`pkg/quota/v1/evaluator/core/*.go`:
//! `podEvaluator`, `pvcEvaluator`, `serviceEvaluator` — everything else
//! real upstream tracks, `Secret`/`ConfigMap`/arbitrary other resource
//! kinds, goes through one shared, fully generic
//! `generic.objectCountEvaluator` keyed on the stable `count/<resource>`
//! (or `count/<resource>.<group>`) convention instead of a per-type
//! `Evaluator`. This crate ports all four: the three specialized ones
//! (compute/storage limits + service-type counts — together what the
//! overwhelming majority of real `ResourceQuota` usage actually targets)
//! and [`check_object_count_create`], the generic one, which needs no
//! per-resource registration decision at all since its `count/...` key
//! is already generic over `(group, resource)` — real upstream's own
//! `kubectl create quota --hard=count/secrets=10` or
//! `count/deployments.apps=5` convention, ported exactly.
//! **Compute/storage/service resources only** — of real upstream's own
//! `podResources`/`pvcResources`/`serviceResources` tracked-resource
//! lists, this port covers `pods` (object count), `cpu`/`requests.cpu`/
//! `limits.cpu`, `memory`/`requests.memory`/`limits.memory`,
//! `ephemeral-storage`/`requests.ephemeral-storage`/
//! `limits.ephemeral-storage`, the `hugepages-<size>`/
//! `requests.hugepages-<size>` prefix family (real upstream's own
//! `podResourcePrefixes`/`requestedResourcePrefixes` — hugepages have no
//! separate `limits` tracking at all in real upstream either, since a
//! hugepage request and its limit are always equal in a real pod spec),
//! `persistentvolumeclaims` (object count), `requests.storage`,
//! `services` (object count), `services.nodeports`,
//! `services.loadbalancers`, and extended resources in their real
//! `requests.<name>`-only form (e.g. `requests.nvidia.com/gpu` — real
//! upstream's own `isExtendedResourceNameForQuota`/
//! `IsExtendedResourceName`: overcommit isn't supported for extended
//! resources, so only the `requests.`-prefixed quota key is ever
//! recognized, never a bare one), and the PVC evaluator's own real
//! per-storage-class resource name family
//! (`<class>.storageclass.storage.k8s.io/persistentvolumeclaims` and
//! `.../requests.storage` — real upstream's own `V1ResourceByStorageClass`
//! key convention). The PVC and service
//! evaluators only apply to an *unscoped* `ResourceQuota` (`spec.scopes`
//! empty) — real upstream's own `pvcEvaluator.Matches` only consults
//! scopes at all behind the alpha `VolumeAttributesClass` feature gate,
//! and `serviceEvaluator.Matches` never consults scopes at all (no
//! feature gate involved for services either way), so this matches both
//! evaluators' real stable behavior, not a shortcut.
//!
//! **`spec.scopes` AND `spec.scopeSelector` matching, all six real scope
//! names** — real upstream's own `ResourceQuota.spec.scopes` lets an
//! operator target a quota at a subset of pods; a pod must match *every*
//! listed scope for the quota to apply (real upstream's own
//! all-must-match semantics, [`quota_matches_pod_scopes`]):
//! `Terminating`/`NotTerminating` (real upstream's own `IsTerminating`:
//! `spec.activeDeadlineSeconds` is set and non-negative),
//! `BestEffort`/`NotBestEffort` (real upstream's own `ComputePodQOS` —
//! see [`compute_pod_qos`]'s own doc comment for exactly what's ported),
//! `PriorityClass` (real upstream's own `podMatchesScopeFunc`: the
//! classic `spec.scopes` list form implies an `Exists` operator, so
//! that route alone is genuinely just "does the pod have *any* priority
//! class name set"), and `CrossNamespacePodAffinity` (real upstream's
//! own `usesCrossNamespacePodAffinity`: a structural presence check
//! across all four real pod-(anti-)affinity term lists for an explicit
//! `namespaces` list or any `namespaceSelector` at all — see
//! [`uses_cross_namespace_pod_affinity`]'s own doc comment). The richer
//! **`spec.scopeSelector.matchExpressions`** form is also ported —
//! `PriorityClass` gains its real `In`/`NotIn`/`DoesNotExist` operators
//! against a specific set of priority class names (real upstream's own
//! `podMatchesSelector`, a plain label-selector match against a
//! synthetic single-key label set), and every other scope name accepts
//! the same expression form with its operator/values ignored, exactly
//! matching real upstream's own per-scope-name switch (see
//! [`scope_requirement_matches`]'s own doc comment).
//!
//! **No persisted `status.used` counter, named honestly**: real
//! upstream's own controller maintains a running `status.used` total on
//! each `ResourceQuota`, updated with real optimistic-concurrency retry
//! as part of admission (`quota.Add(resourceQuota.Status.Used,
//! requestedUsage)`, `quota.LessThanOrEqual` against `Status.Hard`) —
//! this port instead recomputes total usage from scratch on every check
//! (a live `server::rest::list` of every `Pod` in the namespace, summing
//! each non-terminal one's own usage, plus this request's own delta),
//! compared directly against `spec.hard` (there is no `status.hard`
//! sync controller here either, so `spec.hard` is read directly — a real
//! cluster's own `status.hard` converges to exactly that anyway). This is
//! the same "no cache, always live" posture every other plugin in this
//! module already takes, but it carries one real, narrower consequence a
//! persisted counter wouldn't: **a race between two concurrent `CREATE`s
//! that each individually fit under the quota can both be admitted**,
//! together exceeding it — real upstream's own per-quota optimistic-lock
//! retry loop serializes exactly this case; this port doesn't. Named
//! honestly as a real, accepted limitation, not silently glossed over.
//!
//! Real upstream's own denial message format is ported exactly:
//! `"exceeded quota: <name>, requested: <resource>=<val>, used:
//! <resource>=<val>, limited: <resource>=<val>"`, restricted to only the
//! resource(s) that actually exceeded.
//!
//! Reuses [`crate::admission::limit_ranger::pod_requests`]/
//! [`crate::admission::limit_ranger::pod_limits`] for the actual
//! requests/limits aggregation — real upstream's own `PodUsageFunc` calls
//! the exact same `resourcehelper.PodRequests`/`PodLimits` helper
//! `limit_ranger`'s own port already ported once.
//!
//! Same split as every other Group J plugin: pure decision functions
//! (unit tested with no I/O) plus the real I/O steps (`server::rest::list`
//! over `ResourceQuota` and over every `Pod` in the namespace)
//! `server::listener` performs in between.

use crate::admission::limit_ranger::{pod_limits, pod_requests};
use crate::scheme::quantity::Quantity;
use serde_json::Value;
use std::collections::BTreeMap;

pub fn applies_to(
    operation: crate::admission::attributes::Operation,
    group: &str,
    resource: &str,
    subresource: &str,
) -> bool {
    group.is_empty()
        && resource == "pods"
        && subresource.is_empty()
        && operation == crate::admission::attributes::Operation::Create
}

/// Real upstream's own `QuotaV1Pod`, minus the `DeletionGracePeriodSeconds`
/// grace-period-expiry half — this crate's admission path never sees a
/// pod mid-deletion (a `CREATE`/existing-pod-list read, never a delete in
/// progress with a live clock to compare against), so only the terminal-
/// phase check applies in practice; kept as its own named subset rather
/// than silently assumed equivalent.
fn counts_toward_quota(pod: &Value) -> bool {
    let phase = pod
        .get("status")
        .and_then(|s| s.get("phase"))
        .and_then(Value::as_str)
        .unwrap_or("");
    phase != "Failed" && phase != "Succeeded"
}

/// Real upstream's own `IsTerminating`: `spec.activeDeadlineSeconds` is
/// set and non-negative.
fn is_terminating(pod: &Value) -> bool {
    pod.get("spec")
        .and_then(|s| s.get("activeDeadlineSeconds"))
        .and_then(Value::as_i64)
        .is_some_and(|s| s >= 0)
}

fn positive_quantity(container: &Value, field: &str, name: &str) -> Option<Quantity> {
    let raw = container
        .get("resources")?
        .get(field)?
        .get(name)?
        .as_str()?;
    let q = Quantity::parse(raw).ok()?;
    (q > Quantity::ZERO).then_some(q)
}

/// Real upstream's own `ComputePodQOS` (`pkg/apis/core/v1/helper/qos/qos.go`),
/// restricted to the two real QoS-relevant resources upstream itself
/// restricts to (`cpu`/`memory` — `isSupportedQoSComputeResource`), and
/// without the `PodLevelResources` feature-gate branch (alpha, no
/// feature-gate machinery in this crate — the per-container branch,
/// upstream's own default, is always used). Deliberately **not** the
/// same aggregation as [`pod_requests`]/[`pod_limits`] — QoS classification
/// is a simpler per-container "does every container set both cpu and
/// memory limits, matching its own requests" check, not the pod-wide
/// sidecar-aware total those two compute.
fn compute_pod_qos(pod: &Value) -> &'static str {
    let mut requests: BTreeMap<&str, Quantity> = BTreeMap::new();
    let mut limits: BTreeMap<&str, Quantity> = BTreeMap::new();
    let mut is_guaranteed = true;

    let containers = pod
        .get("spec")
        .and_then(|s| s.get("containers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let init_containers = pod
        .get("spec")
        .and_then(|s| s.get("initContainers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for container in containers.chain(init_containers) {
        for name in ["cpu", "memory"] {
            if let Some(q) = positive_quantity(container, "requests", name) {
                requests
                    .entry(name)
                    .and_modify(|e| *e = *e + q)
                    .or_insert(q);
            }
        }
        let mut limits_found = 0;
        for name in ["cpu", "memory"] {
            if let Some(q) = positive_quantity(container, "limits", name) {
                limits_found += 1;
                limits.entry(name).and_modify(|e| *e = *e + q).or_insert(q);
            }
        }
        if limits_found != 2 {
            is_guaranteed = false;
        }
    }

    if requests.is_empty() && limits.is_empty() {
        return "BestEffort";
    }
    if is_guaranteed {
        for (name, req) in &requests {
            match limits.get(name) {
                Some(lim) if *lim == *req => {}
                _ => {
                    is_guaranteed = false;
                    break;
                }
            }
        }
    }
    if is_guaranteed && requests.len() == limits.len() {
        "Guaranteed"
    } else {
        "Burstable"
    }
}

/// Whether `pod` matches one real `ResourceQuota.spec.scopes` entry — all
/// six real scope names are evaluated (see this module's own doc
/// comment); any genuinely unrecognized scope name (not a real upstream
/// value at all) auto-matches rather than narrowing, the same "err
/// toward stricter enforcement, not laxer" posture as everything else
/// this module doesn't model. The classic `spec.scopes` list form
/// implies the `Exists` operator for every entry — real upstream's own
/// `getScopeSelectorsFromQuota` synthesizes exactly that
/// (`corev1.ScopeSelectorOpExists`) before handing every scope, from
/// either source, to the same `podMatchesScopeFunc`.
fn scope_matches(scope: &str, pod: &Value) -> bool {
    scope_requirement_matches(scope, "Exists", &[], pod)
}

/// Real upstream's own `podMatchesScopeFunc`, generalized to the full
/// `ScopedResourceSelectorRequirement` shape (`scopeName`/`operator`/
/// `values`) that both `spec.scopes` (via [`scope_matches`]'s implied
/// `Exists`) and `spec.scopeSelector.matchExpressions` (the real,
/// richer per-expression form) reduce to. Every scope name except
/// `PriorityClass` ignores `operator`/`values` entirely, same as real
/// upstream's own switch (`Terminating`/`NotTerminating`/`BestEffort`/
/// `NotBestEffort`/`CrossNamespacePodAffinity` never look at the
/// selector's operator at all). `PriorityClass` is the one real
/// upstream case that does: `Exists` stays the cheap presence check
/// (`podMatchesScopeFunc`'s own short-circuit, no selector parsing);
/// `In`/`NotIn`/`DoesNotExist` fall through to real upstream's own
/// `podMatchesSelector` — a plain label-selector match against a
/// synthetic single-key label set (`{PriorityClass:
/// <pod.spec.priorityClassName>}`, present only when non-empty), ported
/// as real `labels.Selector` semantics: `In` requires the key present
/// with a matching value; `NotIn` matches whenever the key is absent OR
/// its value isn't in `values`; `DoesNotExist` requires the key absent.
fn scope_requirement_matches(
    scope_name: &str,
    operator: &str,
    values: &[&str],
    pod: &Value,
) -> bool {
    match scope_name {
        "Terminating" => is_terminating(pod),
        "NotTerminating" => !is_terminating(pod),
        "BestEffort" => compute_pod_qos(pod) == "BestEffort",
        "NotBestEffort" => compute_pod_qos(pod) != "BestEffort",
        "PriorityClass" => {
            let priority_class = pod
                .get("spec")
                .and_then(|s| s.get("priorityClassName"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            match operator {
                // Real upstream's own short-circuit: no selector
                // parsing needed, just "does the pod have any priority
                // class name set."
                "Exists" => priority_class.is_some(),
                "DoesNotExist" => priority_class.is_none(),
                "In" => priority_class.is_some_and(|pc| values.contains(&pc)),
                "NotIn" => match priority_class {
                    Some(pc) => !values.contains(&pc),
                    None => true,
                },
                // Not a real `ScopeSelectorOperator` value at all — err
                // toward stricter enforcement, not laxer, same posture
                // as an unrecognized scope name below.
                _ => true,
            }
        }
        "CrossNamespacePodAffinity" => uses_cross_namespace_pod_affinity(pod),
        _ => true,
    }
}

/// Real upstream's own `usesCrossNamespacePodAffinity`/
/// `crossNamespacePodAffinityTerm`: a term "crosses namespaces" if it
/// names an explicit `namespaces` list or carries a `namespaceSelector`
/// at all (even one that would only ever match the pod's own namespace —
/// upstream's own check is a structural presence check, not an
/// evaluation of what the selector actually matches), checked across all
/// four real term lists (`podAffinity`/`podAntiAffinity`, each
/// `requiredDuringSchedulingIgnoredDuringExecution`/
/// `preferredDuringSchedulingIgnoredDuringExecution`).
fn uses_cross_namespace_pod_affinity(pod: &Value) -> bool {
    fn term_is_cross_namespace(term: &Value) -> bool {
        let has_namespaces = term
            .get("namespaces")
            .and_then(Value::as_array)
            .is_some_and(|ns| !ns.is_empty());
        let has_namespace_selector = term.get("namespaceSelector").is_some();
        has_namespaces || has_namespace_selector
    }

    let Some(affinity) = pod.get("spec").and_then(|s| s.get("affinity")) else {
        return false;
    };
    for kind in ["podAffinity", "podAntiAffinity"] {
        let Some(section) = affinity.get(kind) else {
            continue;
        };
        let required_matches = section
            .get("requiredDuringSchedulingIgnoredDuringExecution")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(term_is_cross_namespace);
        if required_matches {
            return true;
        }
        // Each `preferred` entry is a `WeightedPodAffinityTerm` — the
        // real term is nested one level deeper, under its own
        // `podAffinityTerm` field.
        let preferred_matches = section
            .get("preferredDuringSchedulingIgnoredDuringExecution")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|w| w.get("podAffinityTerm"))
            .any(term_is_cross_namespace);
        if preferred_matches {
            return true;
        }
    }
    false
}

/// A `ResourceQuota` applies to `pod` only if `pod` matches *every*
/// scope selector the quota carries — real upstream's own
/// `getScopeSelectorsFromQuota` concatenates `spec.scopes` (each
/// synthesized as an implied-`Exists` requirement) with
/// `spec.scopeSelector.matchExpressions` (the real, richer per-
/// expression `scopeName`/`operator`/`values` form) into one list, then
/// requires every entry in it to match (`generic.Matches`'s own
/// `matchScope = matchScope && innerMatch` fold). An absent/empty
/// `spec.scopes` and `spec.scopeSelector` matches every pod, same as
/// before scope matching existed.
fn quota_matches_pod_scopes(resource_quota: &Value, pod: &Value) -> bool {
    let scopes_match = resource_quota
        .get("spec")
        .and_then(|s| s.get("scopes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .all(|scope| scope_matches(scope, pod));
    let selector_matches = resource_quota
        .get("spec")
        .and_then(|s| s.get("scopeSelector"))
        .and_then(|ss| ss.get("matchExpressions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .all(|expr| {
            let scope_name = expr.get("scopeName").and_then(Value::as_str).unwrap_or("");
            let operator = expr
                .get("operator")
                .and_then(Value::as_str)
                .unwrap_or("Exists");
            let values: Vec<&str> = expr
                .get("values")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            scope_requirement_matches(scope_name, operator, &values, pod)
        });
    scopes_match && selector_matches
}

/// Whether `resource_quota` carries any scope selector at all, from
/// either real source (`spec.scopes` or `spec.scopeSelector.matchExpressions`)
/// — used by the evaluators that only ever match an *unscoped* quota
/// (PVC/service/generic object-count; see each's own doc comment for
/// why). Real upstream's own `generic.Matches` folds every entry from
/// both sources through the same `scopeFunc`, so for an evaluator whose
/// `scopeFunc` is `MatchesNoScopeFunc` (always `false`), the presence of
/// *any* entry from *either* source — not just `spec.scopes` — makes
/// `matchScope` false.
fn quota_has_any_scope_selectors(resource_quota: &Value) -> bool {
    let has_scopes = resource_quota
        .get("spec")
        .and_then(|s| s.get("scopes"))
        .and_then(Value::as_array)
        .is_some_and(|s| !s.is_empty());
    let has_scope_selector = resource_quota
        .get("spec")
        .and_then(|s| s.get("scopeSelector"))
        .and_then(|ss| ss.get("matchExpressions"))
        .and_then(Value::as_array)
        .is_some_and(|s| !s.is_empty());
    has_scopes || has_scope_selector
}

/// Real upstream's own `podComputeUsageHelper`, restricted to the
/// compute-resource subset this port covers (see this module's own doc
/// comment). `requests`/`limits` are [`pod_requests`]/[`pod_limits`]'s
/// own aggregated output for one pod.
fn pod_compute_usage(
    requests: &BTreeMap<String, Quantity>,
    limits: &BTreeMap<String, Quantity>,
) -> BTreeMap<String, Quantity> {
    let mut usage = BTreeMap::new();
    usage.insert(
        "pods".to_string(),
        Quantity::parse("1").expect("literal \"1\" always parses"),
    );
    if let Some(&cpu) = requests.get("cpu") {
        usage.insert("cpu".to_string(), cpu);
        usage.insert("requests.cpu".to_string(), cpu);
    }
    if let Some(&cpu) = limits.get("cpu") {
        usage.insert("limits.cpu".to_string(), cpu);
    }
    if let Some(&mem) = requests.get("memory") {
        usage.insert("memory".to_string(), mem);
        usage.insert("requests.memory".to_string(), mem);
    }
    if let Some(&mem) = limits.get("memory") {
        usage.insert("limits.memory".to_string(), mem);
    }
    if let Some(&storage) = requests.get("ephemeral-storage") {
        usage.insert("ephemeral-storage".to_string(), storage);
        usage.insert("requests.ephemeral-storage".to_string(), storage);
    }
    if let Some(&storage) = limits.get("ephemeral-storage") {
        usage.insert("limits.ephemeral-storage".to_string(), storage);
    }
    // Real upstream's own `requestedResourcePrefixes` loop
    // (`podComputeUsageHelper`): every *requested* `hugepages-<size>`
    // resource counts both under its own bare name and under
    // `requests.hugepages-<size>` — hugepages have no separate `limits`
    // tracking at all in real upstream (a hugepage request and its limit
    // are always equal in a real pod spec, so only `requests` is ever
    // consulted here).
    for (name, &q) in requests {
        if let Some(size) = name.strip_prefix("hugepages-") {
            usage.insert(name.clone(), q);
            usage.insert(format!("requests.hugepages-{size}"), q);
        }
    }
    // Real upstream's own extended-resource branch of the same loop:
    // "as overcommit is not supported by extended resources for now,
    // only quota objects in format of `requests.resourceName` is
    // allowed" — so unlike compute/hugepages resources, an extended
    // resource gets *only* its `requests.<name>` entry, never a bare
    // one.
    for (name, &q) in requests {
        if is_extended_resource_name(name) {
            usage.insert(format!("requests.{name}"), q);
        }
    }
    usage
}

/// Real upstream's own `helper.IsExtendedResourceName`, simplified: a
/// resource name that names a real extended resource (e.g.
/// `nvidia.com/gpu`) rather than a native/built-in one. Ported checks:
/// not [`is_native_resource`] (real upstream's own `IsNativeResource` —
/// no `/` at all, or one under the reserved `kubernetes.io/` namespace),
/// and not already `requests.`-prefixed (upstream's own guard against
/// double-prefixing a resource name that's already in quota-key form).
/// **Not ported**: upstream's own final `IsQualifiedName` structural
/// validation of the would-be `requests.<name>` quota key — this port
/// trusts any not-obviously-native name shape rather than re-validating
/// full DNS-subdomain/qualified-name grammar a second time here.
fn is_extended_resource_name(name: &str) -> bool {
    !is_native_resource(name) && !name.starts_with("requests.")
}

/// Real upstream's own `helper.IsNativeResource`: no `/` at all (an
/// unprefixed name is implicitly in the `kubernetes.io/` namespace), or
/// one that's explicitly under the reserved `kubernetes.io/` namespace
/// (`IsPrefixedNativeResource`).
fn is_native_resource(name: &str) -> bool {
    !name.contains('/') || name.contains("kubernetes.io/")
}

/// Real upstream's own `PodUsageFunc`: the object-count entry (`pods`)
/// is always charged, even for a pod that wouldn't otherwise count
/// (real upstream's own comment: "always quota the object count... even
/// if the pod is end of life") — but this port's caller only ever calls
/// this for pods that already passed [`counts_toward_quota`], since a
/// terminal pod contributes nothing else either way here (no separate
/// `count/pods` resource name tracked — see this module's own doc
/// comment for what's out of scope).
pub fn pod_usage(pod: &Value) -> BTreeMap<String, Quantity> {
    pod_compute_usage(&pod_requests(pod), &pod_limits(pod))
}

fn add_maps(
    a: &BTreeMap<String, Quantity>,
    b: &BTreeMap<String, Quantity>,
) -> BTreeMap<String, Quantity> {
    let mut out = a.clone();
    for (k, v) in b {
        out.entry(k.clone())
            .and_modify(|e| *e = *e + *v)
            .or_insert(*v);
    }
    out
}

fn hard_limits(resource_quota: &Value) -> BTreeMap<String, Quantity> {
    resource_quota
        .get("spec")
        .and_then(|s| s.get("hard"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, raw)| Some((name.clone(), Quantity::parse(raw.as_str()?).ok()?)))
        .collect()
}

/// Real upstream's own `MatchingResources`/`Intersection`: whether
/// `resource_quota` tracks at least one resource this pod usage
/// computation could ever produce (`pod_compute_usage`'s own key set) —
/// a `ResourceQuota` whose `spec.hard` only names resources this port
/// doesn't track (e.g. `count/services`) is correctly not consulted at
/// all, rather than spuriously matched.
const TRACKED_RESOURCES: [&str; 10] = [
    "pods",
    "cpu",
    "requests.cpu",
    "limits.cpu",
    "memory",
    "requests.memory",
    "limits.memory",
    "ephemeral-storage",
    "requests.ephemeral-storage",
    "limits.ephemeral-storage",
];

/// Real upstream's own `isExtendedResourceNameForQuota`: a `spec.hard`
/// key names an extended resource's quota entry only in its
/// `requests.<name>` form (real upstream's own comment: "as overcommit
/// is not supported by extended resources for now, only quota objects
/// in format of `requests.resourceName` is allowed") — the *whole* key,
/// prefix included, must not be a native resource name (a `kubernetes.io/`
/// name, once prefixed with `requests.`, would still contain
/// `kubernetes.io/` and correctly not match here).
fn is_extended_resource_hard_key(key: &str) -> bool {
    key.starts_with("requests.") && !is_native_resource(key)
}

fn quota_applies(resource_quota: &Value) -> bool {
    hard_limits(resource_quota).keys().any(|k| {
        TRACKED_RESOURCES.contains(&k.as_str())
            || k.starts_with("hugepages-")
            || k.starts_with("requests.hugepages-")
            || is_extended_resource_hard_key(k)
    })
}

/// Real upstream's own `pvcResources`.
const TRACKED_PVC_RESOURCES: [&str; 2] = ["persistentvolumeclaims", "requests.storage"];

/// Real upstream's own `storageClassSuffix`
/// (`pkg/quota/v1/evaluator/core/persistent_volume_claims.go`): a
/// storage-class-scoped quota key is `<storage-class-name>` plus this
/// suffix plus one of [`TRACKED_PVC_RESOURCES`], e.g.
/// `gold.storageclass.storage.k8s.io/requests.storage`.
const STORAGE_CLASS_SUFFIX: &str = ".storageclass.storage.k8s.io/";

/// Real upstream's own `MatchingResources`' third branch: a `spec.hard`
/// key scoped to a storage class matches if it ends with the suffix form
/// of one of [`TRACKED_PVC_RESOURCES`] — the storage class name itself
/// isn't checked against anything (any prefix, including empty, is
/// accepted, same as real upstream's own plain `strings.HasSuffix`).
fn is_storage_class_scoped_pvc_key(key: &str) -> bool {
    TRACKED_PVC_RESOURCES
        .iter()
        .any(|r| key.ends_with(&format!("{STORAGE_CLASS_SUFFIX}{r}")))
}

fn quota_applies_to_pvcs(resource_quota: &Value) -> bool {
    hard_limits(resource_quota)
        .keys()
        .any(|k| TRACKED_PVC_RESOURCES.contains(&k.as_str()) || is_storage_class_scoped_pvc_key(k))
}

/// Real upstream's own `storagehelpers.GetPersistentVolumeClaimClass`
/// (`k8s.io/component-helpers/storage/volume`): the beta
/// `volume.beta.kubernetes.io/storage-class` annotation takes precedence
/// over `spec.storageClassName` when both are set — the same precedence
/// `admission::default_storage_class`'s own `pvc_has_class` already
/// ported for a different purpose (whether a PVC has *any* class at
/// all); this is its value-returning counterpart.
fn pvc_storage_class_ref(pvc: &Value) -> Option<String> {
    if let Some(class) = pvc
        .get("metadata")
        .and_then(|m| m.get("annotations"))
        .and_then(|a| a.get("volume.beta.kubernetes.io/storage-class"))
        .and_then(Value::as_str)
    {
        return Some(class.to_string());
    }
    pvc.get("spec")
        .and_then(|s| s.get("storageClassName"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Real upstream's own `pvcEvaluator.Usage`: `persistentvolumeclaims`
/// (count) and `requests.storage` are always charged; when the claim
/// names a storage class ([`pvc_storage_class_ref`]), both are charged a
/// second time under that class's own scoped key
/// (`V1ResourceByStorageClass`). Not ported: the
/// `RecoverVolumeExpansionFailure`-gated `status.allocatedResources`
/// comparison — this always uses the plain
/// `spec.resources.requests.storage` value, no feature-gate machinery to
/// model the alpha/beta variant.
fn pvc_usage(pvc: &Value) -> BTreeMap<String, Quantity> {
    let mut usage = BTreeMap::new();
    let one = Quantity::parse("1").expect("literal \"1\" always parses");
    usage.insert("persistentvolumeclaims".to_string(), one);
    let storage_class = pvc_storage_class_ref(pvc);
    if let Some(class) = &storage_class {
        usage.insert(
            format!("{class}{STORAGE_CLASS_SUFFIX}persistentvolumeclaims"),
            one,
        );
    }
    if let Some(q) = pvc
        .get("spec")
        .and_then(|s| s.get("resources"))
        .and_then(|r| r.get("requests"))
        .and_then(|r| r.get("storage"))
        .and_then(Value::as_str)
        .and_then(|s| Quantity::parse(s).ok())
    {
        usage.insert("requests.storage".to_string(), q);
        if let Some(class) = &storage_class {
            usage.insert(format!("{class}{STORAGE_CLASS_SUFFIX}requests.storage"), q);
        }
    }
    usage
}

fn format_usage(map: &BTreeMap<String, Quantity>) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Real upstream's own `checkRequest` core comparison, ported: for one
/// `ResourceQuota`, would `new_total` (this pod's own usage, added to
/// every other non-terminal pod's usage already in the namespace) exceed
/// `spec.hard` for any resource the quota actually tracks? `None` if not
/// — real upstream's own message format
/// (`"exceeded quota: <name>, requested: ..., used: ..., limited:
/// ..."`), restricted to only the resource(s) that actually exceeded.
fn check_quota(
    resource_quota: &Value,
    existing_usage: &BTreeMap<String, Quantity>,
    this_pod_usage: &BTreeMap<String, Quantity>,
) -> Option<String> {
    let hard = hard_limits(resource_quota);
    let new_total = add_maps(existing_usage, this_pod_usage);

    let mut exceeded_requested = BTreeMap::new();
    let mut exceeded_used = BTreeMap::new();
    let mut exceeded_hard = BTreeMap::new();
    for (name, &limit) in &hard {
        let Some(&requested) = this_pod_usage.get(name) else {
            continue;
        };
        let total = new_total.get(name).copied().unwrap_or(Quantity::ZERO);
        if total > limit {
            exceeded_requested.insert(name.clone(), requested);
            exceeded_used.insert(
                name.clone(),
                existing_usage.get(name).copied().unwrap_or(Quantity::ZERO),
            );
            exceeded_hard.insert(name.clone(), limit);
        }
    }
    if exceeded_requested.is_empty() {
        return None;
    }
    let name = resource_quota
        .get("metadata")
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(format!(
        "exceeded quota: {name}, requested: {}, used: {}, limited: {}",
        format_usage(&exceeded_requested),
        format_usage(&exceeded_used),
        format_usage(&exceeded_hard)
    ))
}

/// The full decision for one pod `CREATE`: `existing_pods` is every
/// other `Pod` already in the namespace (`server::rest::list`'s own
/// output, before this new one is written) — terminal ones
/// ([`counts_toward_quota`]) are excluded from the summed usage, same as
/// real upstream. `resource_quotas` is every `ResourceQuota` in the
/// namespace; a quota is only checked if it [`quota_applies`] (tracks a
/// resource this port covers) *and* [`quota_matches_pod_scopes`] against
/// this specific pod (real upstream's own per-quota scope matching — a
/// quota scoped to `BestEffort` only sums *other* `BestEffort` pods'
/// usage too, not every pod in the namespace, which is why the existing-
/// usage sum is computed per-quota here rather than once globally).
/// Returns the first quota's own denial message, real upstream's own
/// "the first `ResourceQuota` that would be exceeded wins" posture (its
/// own loop returns on the first failure, doesn't aggregate every
/// quota's own violations the way `LimitRanger`'s per-`LimitRange`
/// checks do).
pub fn check_pod_create(
    pod: &Value,
    existing_pods: &[Value],
    resource_quotas: &[Value],
) -> Option<String> {
    let this_pod_usage = pod_usage(pod);

    for resource_quota in resource_quotas {
        if !quota_applies(resource_quota) || !quota_matches_pod_scopes(resource_quota, pod) {
            continue;
        }
        let mut existing_usage = BTreeMap::new();
        for existing in existing_pods {
            if counts_toward_quota(existing) && quota_matches_pod_scopes(resource_quota, existing) {
                existing_usage = add_maps(&existing_usage, &pod_usage(existing));
            }
        }
        if let Some(message) = check_quota(resource_quota, &existing_usage, &this_pod_usage) {
            return Some(message);
        }
    }
    None
}

/// The persisted-`status.used` half of [`check_pod_create`]: once a
/// request is admitted, this is the post-create usage total for every
/// quota `check_pod_create` considered — real upstream's own admission
/// plugin persists exactly this via an optimistic-concurrency retry loop
/// (`quotaAccessor.UpdateQuotaStatus`), the piece this crate's own
/// `resource_quota` doc comment used to name as its one remaining gap.
/// Kept as a separate function rather than folding into
/// `check_pod_create` itself: computing it is wasted work on the
/// overwhelmingly common "just tell me allow/deny" call, including this
/// module's own existing unit tests. Returns `(quota_name, new_total)`
/// pairs — `new_total` only carries the keys this pod evaluator itself
/// tracks (`pod_usage`'s own keys), not a full merge with whatever a
/// quota's `status.used` might already hold for other evaluators'
/// resources (PVC storage, service counts, ...) — merging onto the
/// existing persisted value without clobbering those is the caller's
/// job (`server::listener`'s own persist step reads-modifies-writes).
pub fn usage_after_pod_create(
    pod: &Value,
    existing_pods: &[Value],
    resource_quotas: &[Value],
) -> Vec<(String, BTreeMap<String, Quantity>)> {
    let this_pod_usage = pod_usage(pod);
    let mut updates = Vec::new();

    for resource_quota in resource_quotas {
        if !quota_applies(resource_quota) || !quota_matches_pod_scopes(resource_quota, pod) {
            continue;
        }
        let mut existing_usage = BTreeMap::new();
        for existing in existing_pods {
            if counts_toward_quota(existing) && quota_matches_pod_scopes(resource_quota, existing) {
                existing_usage = add_maps(&existing_usage, &pod_usage(existing));
            }
        }
        let new_total = add_maps(&existing_usage, &this_pod_usage);
        let name = resource_quota
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        updates.push((name, new_total));
    }
    updates
}

pub fn applies_to_pvc(
    operation: crate::admission::attributes::Operation,
    group: &str,
    resource: &str,
    subresource: &str,
) -> bool {
    group.is_empty()
        && resource == "persistentvolumeclaims"
        && subresource.is_empty()
        && operation == crate::admission::attributes::Operation::Create
}

/// Real upstream's own `pvcEvaluator`, ported: [`pvc_usage`] tracks
/// `persistentvolumeclaims` (count) and `requests.storage`, plus both
/// again under the claim's own storage class's scoped key
/// (`<class>.storageclass.storage.k8s.io/...`) when it names one.
/// **Unscoped
/// quotas only** — real upstream's own `pvcEvaluator.Matches` only
/// consults `spec.scopes`/`spec.scopeSelector` at all behind the alpha
/// `VolumeAttributesClass` feature gate (this crate has no feature-gate
/// machinery to model that), so a `ResourceQuota` carrying any
/// `spec.scopes` entries is treated as not applying to `PersistentVolumeClaim`s
/// at all here — real upstream's own stable-feature-gate default
/// behavior for exactly this case (`generic.MatchesNoScopeFunc`).
pub fn check_pvc_create(
    pvc: &Value,
    existing_pvcs: &[Value],
    resource_quotas: &[Value],
) -> Option<String> {
    let this_pvc_usage = pvc_usage(pvc);

    for resource_quota in resource_quotas {
        let has_scopes = quota_has_any_scope_selectors(resource_quota);
        if has_scopes || !quota_applies_to_pvcs(resource_quota) {
            continue;
        }
        let mut existing_usage = BTreeMap::new();
        for existing in existing_pvcs {
            existing_usage = add_maps(&existing_usage, &pvc_usage(existing));
        }
        if let Some(message) = check_quota(resource_quota, &existing_usage, &this_pvc_usage) {
            return Some(message);
        }
    }
    None
}

/// The persisted-`status.used` half of [`check_pvc_create`] — see
/// [`usage_after_pod_create`]'s own doc comment for the shape and
/// reasoning (this is the same pattern applied to the PVC evaluator, its
/// own named follow-up).
pub fn usage_after_pvc_create(
    pvc: &Value,
    existing_pvcs: &[Value],
    resource_quotas: &[Value],
) -> Vec<(String, BTreeMap<String, Quantity>)> {
    let this_pvc_usage = pvc_usage(pvc);
    let mut updates = Vec::new();

    for resource_quota in resource_quotas {
        let has_scopes = quota_has_any_scope_selectors(resource_quota);
        if has_scopes || !quota_applies_to_pvcs(resource_quota) {
            continue;
        }
        let mut existing_usage = BTreeMap::new();
        for existing in existing_pvcs {
            existing_usage = add_maps(&existing_usage, &pvc_usage(existing));
        }
        let new_total = add_maps(&existing_usage, &this_pvc_usage);
        let name = resource_quota
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        updates.push((name, new_total));
    }
    updates
}

include!("resource_quota_services.rs");
include!("resource_quota_tests.rs");
