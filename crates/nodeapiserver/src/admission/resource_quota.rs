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
//! `services.loadbalancers`; not ported: extended resources
//! (`isExtendedResourceNameForQuota`) and the PVC evaluator's own real
//! per-storage-class resource name family
//! (`<class>.storageclass.storage.k8s.io/...`) — both real, both just
//! not this crate's first cut. The PVC and service
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
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn applies_to(operation: crate::admission::attributes::Operation, group: &str, resource: &str, subresource: &str) -> bool {
    group.is_empty() && resource == "pods" && subresource.is_empty() && operation == crate::admission::attributes::Operation::Create
}

/// Real upstream's own `QuotaV1Pod`, minus the `DeletionGracePeriodSeconds`
/// grace-period-expiry half — this crate's admission path never sees a
/// pod mid-deletion (a `CREATE`/existing-pod-list read, never a delete in
/// progress with a live clock to compare against), so only the terminal-
/// phase check applies in practice; kept as its own named subset rather
/// than silently assumed equivalent.
fn counts_toward_quota(pod: &Value) -> bool {
    let phase = pod.get("status").and_then(|s| s.get("phase")).and_then(Value::as_str).unwrap_or("");
    phase != "Failed" && phase != "Succeeded"
}

/// Real upstream's own `IsTerminating`: `spec.activeDeadlineSeconds` is
/// set and non-negative.
fn is_terminating(pod: &Value) -> bool {
    pod.get("spec").and_then(|s| s.get("activeDeadlineSeconds")).and_then(Value::as_i64).is_some_and(|s| s >= 0)
}

fn positive_quantity(container: &Value, field: &str, name: &str) -> Option<Quantity> {
    let raw = container.get("resources")?.get(field)?.get(name)?.as_str()?;
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

    let containers = pod.get("spec").and_then(|s| s.get("containers")).and_then(Value::as_array).into_iter().flatten();
    let init_containers = pod.get("spec").and_then(|s| s.get("initContainers")).and_then(Value::as_array).into_iter().flatten();
    for container in containers.chain(init_containers) {
        for name in ["cpu", "memory"] {
            if let Some(q) = positive_quantity(container, "requests", name) {
                requests.entry(name).and_modify(|e| *e = *e + q).or_insert(q);
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
fn scope_requirement_matches(scope_name: &str, operator: &str, values: &[&str], pod: &Value) -> bool {
    match scope_name {
        "Terminating" => is_terminating(pod),
        "NotTerminating" => !is_terminating(pod),
        "BestEffort" => compute_pod_qos(pod) == "BestEffort",
        "NotBestEffort" => compute_pod_qos(pod) != "BestEffort",
        "PriorityClass" => {
            let priority_class = pod.get("spec").and_then(|s| s.get("priorityClassName")).and_then(Value::as_str).filter(|s| !s.is_empty());
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
        let has_namespaces = term.get("namespaces").and_then(Value::as_array).is_some_and(|ns| !ns.is_empty());
        let has_namespace_selector = term.get("namespaceSelector").is_some();
        has_namespaces || has_namespace_selector
    }

    let Some(affinity) = pod.get("spec").and_then(|s| s.get("affinity")) else { return false };
    for kind in ["podAffinity", "podAntiAffinity"] {
        let Some(section) = affinity.get(kind) else { continue };
        let required_matches = section.get("requiredDuringSchedulingIgnoredDuringExecution").and_then(Value::as_array).into_iter().flatten().any(term_is_cross_namespace);
        if required_matches {
            return true;
        }
        // Each `preferred` entry is a `WeightedPodAffinityTerm` — the
        // real term is nested one level deeper, under its own
        // `podAffinityTerm` field.
        let preferred_matches = section.get("preferredDuringSchedulingIgnoredDuringExecution").and_then(Value::as_array).into_iter().flatten().filter_map(|w| w.get("podAffinityTerm")).any(term_is_cross_namespace);
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
    let scopes_match = resource_quota.get("spec").and_then(|s| s.get("scopes")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).all(|scope| scope_matches(scope, pod));
    let selector_matches = resource_quota
        .get("spec")
        .and_then(|s| s.get("scopeSelector"))
        .and_then(|ss| ss.get("matchExpressions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .all(|expr| {
            let scope_name = expr.get("scopeName").and_then(Value::as_str).unwrap_or("");
            let operator = expr.get("operator").and_then(Value::as_str).unwrap_or("Exists");
            let values: Vec<&str> = expr.get("values").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
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
    let has_scopes = resource_quota.get("spec").and_then(|s| s.get("scopes")).and_then(Value::as_array).is_some_and(|s| !s.is_empty());
    let has_scope_selector = resource_quota.get("spec").and_then(|s| s.get("scopeSelector")).and_then(|ss| ss.get("matchExpressions")).and_then(Value::as_array).is_some_and(|s| !s.is_empty());
    has_scopes || has_scope_selector
}

/// Real upstream's own `podComputeUsageHelper`, restricted to the
/// compute-resource subset this port covers (see this module's own doc
/// comment). `requests`/`limits` are [`pod_requests`]/[`pod_limits`]'s
/// own aggregated output for one pod.
fn pod_compute_usage(requests: &BTreeMap<String, Quantity>, limits: &BTreeMap<String, Quantity>) -> BTreeMap<String, Quantity> {
    let mut usage = BTreeMap::new();
    usage.insert("pods".to_string(), Quantity::parse("1").expect("literal \"1\" always parses"));
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
    usage
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

fn add_maps(a: &BTreeMap<String, Quantity>, b: &BTreeMap<String, Quantity>) -> BTreeMap<String, Quantity> {
    let mut out = a.clone();
    for (k, v) in b {
        out.entry(k.clone()).and_modify(|e| *e = *e + *v).or_insert(*v);
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
const TRACKED_RESOURCES: [&str; 10] = ["pods", "cpu", "requests.cpu", "limits.cpu", "memory", "requests.memory", "limits.memory", "ephemeral-storage", "requests.ephemeral-storage", "limits.ephemeral-storage"];

fn quota_applies(resource_quota: &Value) -> bool {
    hard_limits(resource_quota).keys().any(|k| TRACKED_RESOURCES.contains(&k.as_str()) || k.starts_with("hugepages-") || k.starts_with("requests.hugepages-"))
}

/// Real upstream's own `pvcResources` — this port's subset (see this
/// module's own doc comment: the real per-storage-class resource name
/// family, `<class>.storageclass.storage.k8s.io/...`, isn't tracked).
const TRACKED_PVC_RESOURCES: [&str; 2] = ["persistentvolumeclaims", "requests.storage"];

fn quota_applies_to_pvcs(resource_quota: &Value) -> bool {
    hard_limits(resource_quota).keys().any(|k| TRACKED_PVC_RESOURCES.contains(&k.as_str()))
}

/// Real upstream's own `pvcEvaluator.Usage`, restricted to the two
/// resources this port tracks (see this module's own doc comment for
/// what's not: the per-storage-class resource family, and the
/// `RecoverVolumeExpansionFailure`-gated `status.allocatedResources`
/// comparison — this always uses the plain `spec.resources.requests.storage`
/// value, no feature-gate machinery to model the alpha/beta variant).
fn pvc_usage(pvc: &Value) -> BTreeMap<String, Quantity> {
    let mut usage = BTreeMap::new();
    usage.insert("persistentvolumeclaims".to_string(), Quantity::parse("1").expect("literal \"1\" always parses"));
    if let Some(q) = pvc.get("spec").and_then(|s| s.get("resources")).and_then(|r| r.get("requests")).and_then(|r| r.get("storage")).and_then(Value::as_str).and_then(|s| Quantity::parse(s).ok()) {
        usage.insert("requests.storage".to_string(), q);
    }
    usage
}

fn format_usage(map: &BTreeMap<String, Quantity>) -> String {
    map.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
}

/// Real upstream's own `checkRequest` core comparison, ported: for one
/// `ResourceQuota`, would `new_total` (this pod's own usage, added to
/// every other non-terminal pod's usage already in the namespace) exceed
/// `spec.hard` for any resource the quota actually tracks? `None` if not
/// — real upstream's own message format
/// (`"exceeded quota: <name>, requested: ..., used: ..., limited:
/// ..."`), restricted to only the resource(s) that actually exceeded.
fn check_quota(resource_quota: &Value, existing_usage: &BTreeMap<String, Quantity>, this_pod_usage: &BTreeMap<String, Quantity>) -> Option<String> {
    let hard = hard_limits(resource_quota);
    let new_total = add_maps(existing_usage, this_pod_usage);

    let mut exceeded_requested = BTreeMap::new();
    let mut exceeded_used = BTreeMap::new();
    let mut exceeded_hard = BTreeMap::new();
    for (name, &limit) in &hard {
        let Some(&requested) = this_pod_usage.get(name) else { continue };
        let total = new_total.get(name).copied().unwrap_or(Quantity::ZERO);
        if total > limit {
            exceeded_requested.insert(name.clone(), requested);
            exceeded_used.insert(name.clone(), existing_usage.get(name).copied().unwrap_or(Quantity::ZERO));
            exceeded_hard.insert(name.clone(), limit);
        }
    }
    if exceeded_requested.is_empty() {
        return None;
    }
    let name = resource_quota.get("metadata").and_then(|m| m.get("name")).and_then(Value::as_str).unwrap_or("");
    Some(format!("exceeded quota: {name}, requested: {}, used: {}, limited: {}", format_usage(&exceeded_requested), format_usage(&exceeded_used), format_usage(&exceeded_hard)))
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
pub fn check_pod_create(pod: &Value, existing_pods: &[Value], resource_quotas: &[Value]) -> Option<String> {
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

pub fn applies_to_pvc(operation: crate::admission::attributes::Operation, group: &str, resource: &str, subresource: &str) -> bool {
    group.is_empty() && resource == "persistentvolumeclaims" && subresource.is_empty() && operation == crate::admission::attributes::Operation::Create
}

/// Real upstream's own `pvcEvaluator`, ported: [`pvc_usage`] tracks
/// `persistentvolumeclaims` (count) and `requests.storage`. **Unscoped
/// quotas only** — real upstream's own `pvcEvaluator.Matches` only
/// consults `spec.scopes`/`spec.scopeSelector` at all behind the alpha
/// `VolumeAttributesClass` feature gate (this crate has no feature-gate
/// machinery to model that), so a `ResourceQuota` carrying any
/// `spec.scopes` entries is treated as not applying to `PersistentVolumeClaim`s
/// at all here — real upstream's own stable-feature-gate default
/// behavior for exactly this case (`generic.MatchesNoScopeFunc`).
pub fn check_pvc_create(pvc: &Value, existing_pvcs: &[Value], resource_quotas: &[Value]) -> Option<String> {
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

pub fn applies_to_service(operation: crate::admission::attributes::Operation, group: &str, resource: &str, subresource: &str) -> bool {
    group.is_empty() && resource == "services" && subresource.is_empty() && operation == crate::admission::attributes::Operation::Create
}

const TRACKED_SERVICE_RESOURCES: [&str; 3] = ["services", "services.nodeports", "services.loadbalancers"];

fn quota_applies_to_services(resource_quota: &Value) -> bool {
    hard_limits(resource_quota).keys().any(|k| TRACKED_SERVICE_RESOURCES.contains(&k.as_str()))
}

/// Real upstream's own `serviceEvaluator.Usage`, ported exactly: every
/// `Service` counts once toward `services`; `services.nodeports` counts
/// the number of ports that actually consume a node port (a `NodePort`
/// service always counts every port; a `LoadBalancer` service counts
/// every port unless it explicitly opted out of node-port allocation via
/// `spec.allocateLoadBalancerNodePorts: false`, in which case only ports
/// with an explicit `nodePort` value already set count — real upstream's
/// own `portsWithNodePorts`); `services.loadbalancers` counts 1 only for
/// `LoadBalancer`-type services. Unscoped quotas only, same posture as
/// [`check_pvc_create`] and for the same reason (real upstream's own
/// `serviceEvaluator.Matches` uses `generic.MatchesNoScopeFunc`
/// unconditionally — services never match any scope at all, no feature
/// gate involved).
fn service_usage(svc: &Value) -> BTreeMap<String, Quantity> {
    let mut usage = BTreeMap::new();
    let one = Quantity::parse("1").expect("literal \"1\" always parses");
    usage.insert("services".to_string(), one);

    let ports = svc.get("spec").and_then(|s| s.get("ports")).and_then(Value::as_array).cloned().unwrap_or_default();
    let port_count = Quantity::parse(&ports.len().to_string()).unwrap_or(Quantity::ZERO);
    let ports_with_node_port = || {
        let count = ports.iter().filter(|p| p.get("nodePort").and_then(Value::as_i64).is_some_and(|n| n != 0)).count();
        Quantity::parse(&count.to_string()).unwrap_or(Quantity::ZERO)
    };

    match svc.get("spec").and_then(|s| s.get("type")).and_then(Value::as_str).unwrap_or("ClusterIP") {
        "NodePort" => {
            usage.insert("services.nodeports".to_string(), port_count);
        }
        "LoadBalancer" => {
            let allocates_node_ports = svc.get("spec").and_then(|s| s.get("allocateLoadBalancerNodePorts")).and_then(Value::as_bool).unwrap_or(true);
            usage.insert("services.nodeports".to_string(), if allocates_node_ports { port_count } else { ports_with_node_port() });
            usage.insert("services.loadbalancers".to_string(), one);
        }
        _ => {}
    }
    usage
}

pub fn check_service_create(svc: &Value, existing_services: &[Value], resource_quotas: &[Value]) -> Option<String> {
    let this_service_usage = service_usage(svc);

    for resource_quota in resource_quotas {
        let has_scopes = quota_has_any_scope_selectors(resource_quota);
        if has_scopes || !quota_applies_to_services(resource_quota) {
            continue;
        }
        let mut existing_usage = BTreeMap::new();
        for existing in existing_services {
            existing_usage = add_maps(&existing_usage, &service_usage(existing));
        }
        if let Some(message) = check_quota(resource_quota, &existing_usage, &this_service_usage) {
            return Some(message);
        }
    }
    None
}

/// Real upstream's own `generic.ObjectCountQuotaResourceNameFor`: the
/// stable `count/<resource>` (core group) or `count/<resource>.<group>`
/// (every other group) quota-resource-name convention — the same one a
/// real cluster's own `kubectl create quota ... --hard=count/secrets=10`
/// or `count/deployments.apps=5` relies on.
pub fn count_quota_resource_name(group: &str, resource: &str) -> String {
    if group.is_empty() {
        format!("count/{resource}")
    } else {
        format!("count/{resource}.{group}")
    }
}

/// Real upstream's own generic `objectCountEvaluator` (real upstream
/// registers one of these for essentially every resource kind that
/// isn't a pod/PVC/service — this port doesn't need a per-resource
/// registration decision at all, since the `count/...` key it checks
/// against is fully generic over `(group, resource)`): forbids a
/// `CREATE` of any resource kind that would push the namespace's object
/// count for that kind over a `ResourceQuota`'s own
/// [`count_quota_resource_name`] hard limit, if one is set. Safe to call
/// unconditionally for any resource `CREATE` — a quota with no matching
/// `count/...` key simply has nothing to check, the same "nothing to
/// enforce" no-op every other unmatched case in this module already is.
/// Unscoped quotas only, matching real upstream's own generic evaluator
/// (no scope matching there either). Deliberately not called for pods/
/// PVCs/services in `server::listener` — those already get their own
/// legacy bare-name object-count tracking (`pods`/`persistentvolumeclaims`/
/// `services`) from their own dedicated evaluators above; running this
/// too would just be a redundant, wasted list round trip for those three,
/// not wrong.
pub fn check_object_count_create(group: &str, resource: &str, existing_objects: &[Value], resource_quotas: &[Value]) -> Option<String> {
    let key = count_quota_resource_name(group, resource);
    let one = Quantity::parse("1").expect("literal \"1\" always parses");
    let existing_count = Quantity::parse(&existing_objects.len().to_string()).unwrap_or(Quantity::ZERO);
    let this_usage = BTreeMap::from([(key.clone(), one)]);
    let existing_usage = BTreeMap::from([(key, existing_count)]);

    for resource_quota in resource_quotas {
        let has_scopes = quota_has_any_scope_selectors(resource_quota);
        if has_scopes {
            continue;
        }
        if let Some(message) = check_quota(resource_quota, &existing_usage, &this_usage) {
            return Some(message);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::attributes::Operation;

    fn pod_with_cpu_request(name: &str, cpu: &str) -> Value {
        json!({"metadata": {"name": name}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"cpu": cpu}}}]}})
    }

    fn quota(name: &str, hard: Value) -> Value {
        json!({"metadata": {"name": name}, "spec": {"hard": hard}})
    }

    #[test]
    fn applies_to_pod_create_only() {
        assert!(applies_to(Operation::Create, "", "pods", ""));
        assert!(!applies_to(Operation::Update, "", "pods", ""));
        assert!(!applies_to(Operation::Create, "", "pods", "status"));
        assert!(!applies_to(Operation::Create, "apps", "deployments", ""));
    }

    #[test]
    fn counts_toward_quota_excludes_terminal_pods() {
        assert!(!counts_toward_quota(&json!({"status": {"phase": "Succeeded"}})));
        assert!(!counts_toward_quota(&json!({"status": {"phase": "Failed"}})));
        assert!(counts_toward_quota(&json!({"status": {"phase": "Running"}})));
        assert!(counts_toward_quota(&json!({})), "no status at all (a just-created pod) counts");
    }

    #[test]
    fn pod_usage_always_counts_the_pod_object() {
        let usage = pod_usage(&json!({"spec": {"containers": []}}));
        assert_eq!(usage["pods"].value(), 1);
    }

    #[test]
    fn pod_usage_tracks_cpu_and_memory_requests_and_limits() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"cpu": "100m", "memory": "128Mi"},
            "limits": {"cpu": "200m", "memory": "256Mi"},
        }}]}});
        let usage = pod_usage(&pod);
        assert_eq!(usage["cpu"].milli_value(), 100);
        assert_eq!(usage["requests.cpu"].milli_value(), 100);
        assert_eq!(usage["limits.cpu"].milli_value(), 200);
        assert_eq!(usage["memory"].value(), 128 * 1024 * 1024);
        assert_eq!(usage["limits.memory"].value(), 256 * 1024 * 1024);
    }

    #[test]
    fn pod_usage_tracks_ephemeral_storage_requests_and_limits() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"ephemeral-storage": "1Gi"},
            "limits": {"ephemeral-storage": "2Gi"},
        }}]}});
        let usage = pod_usage(&pod);
        assert_eq!(usage["ephemeral-storage"].value(), 1024 * 1024 * 1024);
        assert_eq!(usage["requests.ephemeral-storage"].value(), 1024 * 1024 * 1024);
        assert_eq!(usage["limits.ephemeral-storage"].value(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn an_ephemeral_storage_quota_is_enforced() {
        let mut pod = pod_with_cpu_request("new", "1m");
        pod["spec"]["containers"][0]["resources"]["requests"]["ephemeral-storage"] = json!("2Gi");
        let existing = json!({"metadata": {"name": "existing"}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"ephemeral-storage": "3Gi"}}}]}});
        let q = quota("ephemeral-quota", json!({"requests.ephemeral-storage": "4Gi"}));
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("2Gi + 3Gi > 4Gi");
        assert!(denial.contains("requests.ephemeral-storage"));
    }

    #[test]
    fn pod_usage_tracks_hugepages_under_both_its_bare_and_requests_prefixed_name() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"hugepages-2Mi": "4Mi"},
            "limits": {"hugepages-2Mi": "4Mi"},
        }}]}});
        let usage = pod_usage(&pod);
        assert_eq!(usage["hugepages-2Mi"].value(), 4 * 1024 * 1024);
        assert_eq!(usage["requests.hugepages-2Mi"].value(), 4 * 1024 * 1024);
        // Real upstream never tracks a separate limits.hugepages-* key.
        assert!(usage.get("limits.hugepages-2Mi").is_none());
    }

    #[test]
    fn a_hugepages_quota_is_enforced() {
        let pod = json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"hugepages-2Mi": "4Mi"}, "limits": {"hugepages-2Mi": "4Mi"}}}]}});
        let existing = json!({"metadata": {"name": "existing"}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"hugepages-2Mi": "6Mi"}, "limits": {"hugepages-2Mi": "6Mi"}}}]}});
        let q = quota("hugepages-quota", json!({"requests.hugepages-2Mi": "8Mi"}));
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("4Mi + 6Mi > 8Mi");
        assert!(denial.contains("requests.hugepages-2Mi"));
    }

    #[test]
    fn a_bare_hugepages_hard_limit_makes_the_quota_apply_too() {
        // spec.hard can name the resource with its bare hugepages-<size>
        // key too (not just the requests.-prefixed form) -- quota_applies
        // must recognize both, matching real upstream's own
        // podResourcePrefixes covering both prefixes.
        let pod = json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1", "resources": {"requests": {"hugepages-2Mi": "999Mi"}}}]}});
        let q = quota("hugepages-quota", json!({"hugepages-2Mi": "1Mi"}));
        assert!(check_pod_create(&pod, &[], &[q]).is_some());
    }

    #[test]
    fn a_pod_within_quota_is_allowed() {
        let pod = pod_with_cpu_request("new", "500m");
        let q = quota("compute-quota", json!({"requests.cpu": "1"}));
        assert!(check_pod_create(&pod, &[], &[q]).is_none());
    }

    #[test]
    fn a_pod_that_would_exceed_quota_is_denied() {
        let pod = pod_with_cpu_request("new", "600m");
        let existing = pod_with_cpu_request("existing", "500m");
        let q = quota("compute-quota", json!({"requests.cpu": "1"}));
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("600m + 500m > 1 core");
        assert!(denial.contains("exceeded quota: compute-quota"));
        assert!(denial.contains("requests.cpu"));
    }

    #[test]
    fn a_terminal_existing_pod_does_not_count_against_quota() {
        let pod = pod_with_cpu_request("new", "600m");
        let mut existing = pod_with_cpu_request("existing", "500m");
        existing["status"] = json!({"phase": "Succeeded"});
        let q = quota("compute-quota", json!({"requests.cpu": "1"}));
        assert!(check_pod_create(&pod, &[existing], &[q]).is_none(), "a Succeeded pod must not count against quota");
    }

    #[test]
    fn a_quota_tracking_an_unrelated_resource_is_not_consulted() {
        let pod = pod_with_cpu_request("new", "999");
        let q = quota("services-quota", json!({"count/services": "5"}));
        assert!(check_pod_create(&pod, &[], &[q]).is_none(), "a quota tracking only count/services should never see a pod-only check");
    }

    #[test]
    fn the_pods_count_limit_is_enforced_too() {
        let pod = pod_with_cpu_request("new", "1m");
        let existing = pod_with_cpu_request("existing", "1m");
        let q = quota("pod-count-quota", json!({"pods": "1"}));
        let denial = check_pod_create(&pod, &[existing], &[q]).expect("2 pods > hard limit of 1");
        assert!(denial.contains("pods="));
    }

    #[test]
    fn the_first_exceeded_quota_wins_not_an_aggregate_of_all() {
        let pod = pod_with_cpu_request("new", "999");
        let q1 = quota("q1", json!({"requests.cpu": "1"}));
        let q2 = quota("q2", json!({"requests.cpu": "1"}));
        let denial = check_pod_create(&pod, &[], &[q1, q2]).unwrap();
        assert!(denial.contains("q1"), "the first quota in the list should be the one reported");
    }

    #[test]
    fn compute_pod_qos_is_besteffort_with_no_requests_or_limits() {
        let pod = json!({"spec": {"containers": [{"name": "c1"}]}});
        assert_eq!(compute_pod_qos(&pod), "BestEffort");
    }

    #[test]
    fn compute_pod_qos_is_guaranteed_when_every_container_matches_requests_to_limits() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"cpu": "100m", "memory": "128Mi"},
            "limits": {"cpu": "100m", "memory": "128Mi"},
        }}]}});
        assert_eq!(compute_pod_qos(&pod), "Guaranteed");
    }

    #[test]
    fn compute_pod_qos_is_burstable_when_requests_and_limits_differ() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"cpu": "100m", "memory": "128Mi"},
            "limits": {"cpu": "200m", "memory": "256Mi"},
        }}]}});
        assert_eq!(compute_pod_qos(&pod), "Burstable");
    }

    #[test]
    fn compute_pod_qos_is_burstable_when_only_some_containers_set_limits() {
        let pod = json!({"spec": {"containers": [
            {"name": "c1", "resources": {"requests": {"cpu": "100m"}, "limits": {"cpu": "100m", "memory": "128Mi"}}},
            {"name": "c2", "resources": {"requests": {"cpu": "100m"}}},
        ]}});
        assert_eq!(compute_pod_qos(&pod), "Burstable");
    }

    #[test]
    fn is_terminating_reflects_active_deadline_seconds() {
        assert!(is_terminating(&json!({"spec": {"activeDeadlineSeconds": 30}})));
        assert!(is_terminating(&json!({"spec": {"activeDeadlineSeconds": 0}})));
        assert!(!is_terminating(&json!({"spec": {}})));
    }

    #[test]
    fn a_besteffort_scoped_quota_does_not_apply_to_a_non_besteffort_pod() {
        // A pod with requests set is Burstable, not BestEffort -- the
        // scope doesn't match, so the quota shouldn't even be consulted,
        // regardless of what its hard limit says.
        let burstable_pod = pod_with_cpu_request("new", "999");
        let q = json!({"metadata": {"name": "besteffort-quota"}, "spec": {"scopes": ["BestEffort"], "hard": {"requests.cpu": "1"}}});
        assert!(check_pod_create(&burstable_pod, &[], &[q]).is_none());
    }

    #[test]
    fn a_besteffort_scoped_quota_excludes_a_guaranteed_existing_pod_from_its_usage() {
        let besteffort_pod = json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1"}]}});
        // Guaranteed: requests match limits exactly -- not BestEffort, so
        // this existing pod must not count toward a BestEffort-scoped
        // quota's usage even though it's a real pod in the namespace.
        let guaranteed_existing = json!({"metadata": {"name": "existing"}, "spec": {"containers": [{"name": "c1", "resources": {
            "requests": {"cpu": "1", "memory": "1Gi"},
            "limits": {"cpu": "1", "memory": "1Gi"},
        }}]}});
        let q = json!({"metadata": {"name": "besteffort-quota"}, "spec": {"scopes": ["BestEffort"], "hard": {"pods": "1"}}});
        // Only the new BestEffort pod counts (1 <= hard 1) -- the
        // Guaranteed existing pod is correctly excluded.
        assert!(check_pod_create(&besteffort_pod, &[guaranteed_existing.clone()], &[q.clone()]).is_none());

        // Add a second BestEffort existing pod -- now 2 BestEffort pods
        // total > hard 1, correctly denied.
        let besteffort_existing = json!({"metadata": {"name": "existing2"}, "spec": {"containers": [{"name": "c1"}]}});
        assert!(check_pod_create(&besteffort_pod, &[guaranteed_existing, besteffort_existing], &[q]).is_some());
    }

    #[test]
    fn a_terminating_scoped_quota_does_not_apply_to_a_non_terminating_pod() {
        let pod = pod_with_cpu_request("new", "999");
        let q = quota("terminating-quota", json!({"requests.cpu": "1"}));
        let mut scoped = q.clone();
        scoped["spec"]["scopes"] = json!(["Terminating"]);
        // The new pod has no activeDeadlineSeconds, so it is NotTerminating
        // -- the Terminating-scoped quota must not apply to it at all,
        // even though 999 cores would otherwise exceed it.
        assert!(check_pod_create(&pod, &[], &[scoped]).is_none());
    }

    #[test]
    fn a_terminating_scoped_quota_applies_to_a_terminating_pod() {
        let mut pod = pod_with_cpu_request("new", "999");
        pod["spec"]["activeDeadlineSeconds"] = json!(30);
        let mut q = quota("terminating-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["Terminating"]);
        assert!(check_pod_create(&pod, &[], &[q]).is_some());
    }

    #[test]
    fn a_genuinely_unrecognized_scope_name_does_not_narrow_the_quota() {
        let pod = pod_with_cpu_request("new", "999");
        let mut q = quota("future-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["SomeFutureScope"]);
        assert!(check_pod_create(&pod, &[], &[q]).is_some(), "a genuinely unrecognized scope must not exempt the pod from an otherwise-applicable quota");
    }

    #[test]
    fn a_crossnamespaceaffinity_scoped_quota_does_not_apply_without_a_cross_namespace_term() {
        let pod = pod_with_cpu_request("new", "999");
        let mut q = quota("affinity-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["CrossNamespacePodAffinity"]);
        assert!(check_pod_create(&pod, &[], &[q]).is_none(), "a pod with no affinity at all is not in scope for this quota");
    }

    #[test]
    fn a_crossnamespaceaffinity_scoped_quota_applies_when_a_required_term_names_namespaces() {
        let mut pod = pod_with_cpu_request("new", "999");
        pod["spec"]["affinity"] = json!({"podAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [{"namespaces": ["other-ns"], "topologyKey": "kubernetes.io/hostname"}]}});
        let mut q = quota("affinity-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["CrossNamespacePodAffinity"]);
        assert!(check_pod_create(&pod, &[], &[q]).is_some());
    }

    #[test]
    fn a_crossnamespaceaffinity_scoped_quota_applies_for_a_preferred_antiaffinity_namespace_selector() {
        let mut pod = pod_with_cpu_request("new", "999");
        pod["spec"]["affinity"] = json!({"podAntiAffinity": {"preferredDuringSchedulingIgnoredDuringExecution": [
            {"weight": 1, "podAffinityTerm": {"namespaceSelector": {}, "topologyKey": "kubernetes.io/hostname"}},
        ]}});
        let mut q = quota("affinity-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["CrossNamespacePodAffinity"]);
        assert!(check_pod_create(&pod, &[], &[q]).is_some());
    }

    #[test]
    fn an_ordinary_same_namespace_affinity_term_does_not_count_as_cross_namespace() {
        let mut pod = pod_with_cpu_request("new", "999");
        pod["spec"]["affinity"] = json!({"podAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [{"topologyKey": "kubernetes.io/hostname"}]}});
        let mut q = quota("affinity-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["CrossNamespacePodAffinity"]);
        assert!(check_pod_create(&pod, &[], &[q]).is_none(), "a term with no namespaces/namespaceSelector must not count as cross-namespace");
    }

    #[test]
    fn a_priorityclass_scoped_quota_applies_only_to_pods_with_a_priority_class() {
        let mut with_priority = pod_with_cpu_request("new", "999");
        with_priority["spec"]["priorityClassName"] = json!("high");
        let mut q = quota("priority-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopes"] = json!(["PriorityClass"]);
        assert!(check_pod_create(&with_priority, &[], &[q.clone()]).is_some(), "a pod with a priority class name must be checked");

        let without_priority = pod_with_cpu_request("new", "999");
        assert!(check_pod_create(&without_priority, &[], &[q]).is_none(), "a pod with no priority class name is out of scope for this quota");
    }

    fn pvc_with_storage(name: &str, storage: &str) -> Value {
        json!({"metadata": {"name": name}, "spec": {"resources": {"requests": {"storage": storage}}}})
    }

    #[test]
    fn applies_to_pvc_create_only() {
        use crate::admission::attributes::Operation;
        assert!(applies_to_pvc(Operation::Create, "", "persistentvolumeclaims", ""));
        assert!(!applies_to_pvc(Operation::Update, "", "persistentvolumeclaims", ""));
        assert!(!applies_to_pvc(Operation::Create, "", "persistentvolumeclaims", "status"));
    }

    #[test]
    fn pvc_usage_always_counts_the_claim_object() {
        let usage = pvc_usage(&json!({"spec": {}}));
        assert_eq!(usage["persistentvolumeclaims"].value(), 1);
        assert!(usage.get("requests.storage").is_none());
    }

    #[test]
    fn pvc_usage_tracks_requested_storage() {
        let usage = pvc_usage(&pvc_with_storage("data", "10Gi"));
        assert_eq!(usage["requests.storage"].value(), 10 * 1024 * 1024 * 1024);
    }

    #[test]
    fn a_pvc_within_storage_quota_is_allowed() {
        let pvc = pvc_with_storage("new", "5Gi");
        let q = quota("storage-quota", json!({"requests.storage": "10Gi"}));
        assert!(check_pvc_create(&pvc, &[], &[q]).is_none());
    }

    #[test]
    fn a_pvc_that_would_exceed_storage_quota_is_denied() {
        let pvc = pvc_with_storage("new", "8Gi");
        let existing = pvc_with_storage("existing", "5Gi");
        let q = quota("storage-quota", json!({"requests.storage": "10Gi"}));
        let denial = check_pvc_create(&pvc, &[existing], &[q]).expect("8Gi + 5Gi > 10Gi");
        assert!(denial.contains("exceeded quota: storage-quota"));
        assert!(denial.contains("requests.storage"));
    }

    #[test]
    fn the_pvc_count_limit_is_enforced_too() {
        let pvc = pvc_with_storage("new", "1Gi");
        let existing = pvc_with_storage("existing", "1Gi");
        let q = quota("pvc-count-quota", json!({"persistentvolumeclaims": "1"}));
        let denial = check_pvc_create(&pvc, &[existing], &[q]).expect("2 PVCs > hard limit of 1");
        assert!(denial.contains("persistentvolumeclaims="));
    }

    #[test]
    fn a_pvc_quota_tracking_an_unrelated_resource_is_not_consulted() {
        let pvc = pvc_with_storage("new", "999Gi");
        let q = quota("pods-quota", json!({"pods": "5"}));
        assert!(check_pvc_create(&pvc, &[], &[q]).is_none());
    }

    #[test]
    fn a_scoped_quota_does_not_apply_to_pvcs_at_all() {
        let pvc = pvc_with_storage("new", "999Gi");
        let mut q = quota("scoped-quota", json!({"requests.storage": "1Gi"}));
        q["spec"]["scopes"] = json!(["BestEffort"]);
        assert!(check_pvc_create(&pvc, &[], &[q]).is_none(), "a scoped quota must not apply to PVCs at all, matching real upstream's stable-feature-gate-off default");
    }

    fn cluster_ip_service(name: &str) -> Value {
        json!({"metadata": {"name": name}, "spec": {"type": "ClusterIP", "ports": [{"port": 80}]}})
    }

    #[test]
    fn applies_to_service_create_only() {
        assert!(applies_to_service(Operation::Create, "", "services", ""));
        assert!(!applies_to_service(Operation::Update, "", "services", ""));
        assert!(!applies_to_service(Operation::Create, "", "services", "status"));
    }

    #[test]
    fn service_usage_always_counts_the_service_object() {
        let usage = service_usage(&cluster_ip_service("svc"));
        assert_eq!(usage["services"].value(), 1);
        assert!(usage.get("services.nodeports").is_none());
        assert!(usage.get("services.loadbalancers").is_none());
    }

    #[test]
    fn service_usage_counts_nodeports_for_a_nodeport_service() {
        let svc = json!({"metadata": {"name": "svc"}, "spec": {"type": "NodePort", "ports": [{"port": 80}, {"port": 443}]}});
        let usage = service_usage(&svc);
        assert_eq!(usage["services.nodeports"].value(), 2);
        assert!(usage.get("services.loadbalancers").is_none());
    }

    #[test]
    fn service_usage_counts_loadbalancer_and_nodeports_by_default() {
        let svc = json!({"metadata": {"name": "svc"}, "spec": {"type": "LoadBalancer", "ports": [{"port": 80}]}});
        let usage = service_usage(&svc);
        assert_eq!(usage["services.loadbalancers"].value(), 1);
        assert_eq!(usage["services.nodeports"].value(), 1);
    }

    #[test]
    fn service_usage_only_counts_explicit_nodeports_when_allocation_is_disabled() {
        let svc = json!({"metadata": {"name": "svc"}, "spec": {
            "type": "LoadBalancer",
            "allocateLoadBalancerNodePorts": false,
            "ports": [{"port": 80, "nodePort": 30080}, {"port": 443}],
        }});
        let usage = service_usage(&svc);
        assert_eq!(usage["services.nodeports"].value(), 1, "only the port with an explicit nodePort counts");
    }

    #[test]
    fn a_service_within_quota_is_allowed() {
        let svc = cluster_ip_service("new");
        let q = quota("service-quota", json!({"services": "5"}));
        assert!(check_service_create(&svc, &[], &[q]).is_none());
    }

    #[test]
    fn a_nodeport_service_that_would_exceed_quota_is_denied() {
        let svc = json!({"metadata": {"name": "new"}, "spec": {"type": "NodePort", "ports": [{"port": 80}, {"port": 443}]}});
        let existing = json!({"metadata": {"name": "existing"}, "spec": {"type": "NodePort", "ports": [{"port": 8080}]}});
        let q = quota("nodeport-quota", json!({"services.nodeports": "2"}));
        let denial = check_service_create(&svc, &[existing], &[q]).expect("2 + 1 > 2");
        assert!(denial.contains("services.nodeports"));
    }

    #[test]
    fn a_scoped_quota_does_not_apply_to_services_at_all() {
        let svc = cluster_ip_service("new");
        let mut q = quota("scoped-quota", json!({"services": "0"}));
        q["spec"]["scopes"] = json!(["BestEffort"]);
        assert!(check_service_create(&svc, &[], &[q]).is_none());
    }

    #[test]
    fn count_quota_resource_name_uses_the_real_convention() {
        assert_eq!(count_quota_resource_name("", "secrets"), "count/secrets");
        assert_eq!(count_quota_resource_name("apps", "deployments"), "count/deployments.apps");
    }

    #[test]
    fn object_count_quota_allows_a_create_within_the_hard_limit() {
        let q = quota("secrets-quota", json!({"count/secrets": "5"}));
        let existing: Vec<Value> = (0..3).map(|_| json!({})).collect();
        assert!(check_object_count_create("", "secrets", &existing, &[q]).is_none());
    }

    #[test]
    fn object_count_quota_denies_a_create_that_would_exceed_the_hard_limit() {
        let q = quota("secrets-quota", json!({"count/secrets": "5"}));
        let existing: Vec<Value> = (0..5).map(|_| json!({})).collect();
        let denial = check_object_count_create("", "secrets", &existing, &[q]).expect("5 existing + 1 new > hard limit of 5");
        assert!(denial.contains("count/secrets"));
    }

    #[test]
    fn object_count_quota_uses_the_group_qualified_key_for_a_non_core_resource() {
        let q = quota("deploy-quota", json!({"count/deployments.apps": "1"}));
        let existing = vec![json!({})];
        let denial = check_object_count_create("apps", "deployments", &existing, &[q]).expect("1 existing + 1 new > hard limit of 1");
        assert!(denial.contains("count/deployments.apps"));
    }

    #[test]
    fn object_count_quota_ignores_an_unrelated_hard_limit() {
        let q = quota("secrets-quota", json!({"count/configmaps": "1"}));
        let existing = vec![json!({}), json!({}), json!({})];
        assert!(check_object_count_create("", "secrets", &existing, &[q]).is_none());
    }

    #[test]
    fn object_count_quota_does_not_apply_when_scoped() {
        let mut q = quota("secrets-quota", json!({"count/secrets": "0"}));
        q["spec"]["scopes"] = json!(["BestEffort"]);
        assert!(check_object_count_create("", "secrets", &[], &[q]).is_none());
    }

    #[test]
    fn a_scope_selector_priority_class_in_matches_only_the_named_classes() {
        let mut high = pod_with_cpu_request("new", "999");
        high["spec"]["priorityClassName"] = json!("high");
        let mut q = quota("priority-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopeSelector"] = json!({"matchExpressions": [{"scopeName": "PriorityClass", "operator": "In", "values": ["high", "critical"]}]});
        assert!(check_pod_create(&high, &[], &[q.clone()]).is_some(), "priorityClassName=high is in the In list, so the quota applies");

        let mut low = pod_with_cpu_request("new", "999");
        low["spec"]["priorityClassName"] = json!("low");
        assert!(check_pod_create(&low, &[], &[q]).is_none(), "priorityClassName=low is not in the In list, so the quota must not apply");
    }

    #[test]
    fn a_scope_selector_priority_class_notin_matches_absent_or_unlisted() {
        let mut q = quota("priority-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopeSelector"] = json!({"matchExpressions": [{"scopeName": "PriorityClass", "operator": "NotIn", "values": ["high"]}]});

        let no_priority = pod_with_cpu_request("new", "999");
        assert!(check_pod_create(&no_priority, &[], &[q.clone()]).is_some(), "no priority class name at all: NotIn matches (key absent)");

        let mut low = pod_with_cpu_request("new", "999");
        low["spec"]["priorityClassName"] = json!("low");
        assert!(check_pod_create(&low, &[], &[q.clone()]).is_some(), "priorityClassName=low is not in the disallowed set, so NotIn matches");

        let mut high = pod_with_cpu_request("new", "999");
        high["spec"]["priorityClassName"] = json!("high");
        assert!(check_pod_create(&high, &[], &[q]).is_none(), "priorityClassName=high is in the disallowed set, so NotIn must not match");
    }

    #[test]
    fn a_scope_selector_priority_class_doesnotexist_requires_no_priority_class() {
        let mut q = quota("priority-quota", json!({"requests.cpu": "1"}));
        q["spec"]["scopeSelector"] = json!({"matchExpressions": [{"scopeName": "PriorityClass", "operator": "DoesNotExist"}]});

        let no_priority = pod_with_cpu_request("new", "999");
        assert!(check_pod_create(&no_priority, &[], &[q.clone()]).is_some());

        let mut with_priority = pod_with_cpu_request("new", "999");
        with_priority["spec"]["priorityClassName"] = json!("high");
        assert!(check_pod_create(&with_priority, &[], &[q]).is_none());
    }

    #[test]
    fn scopes_and_scope_selector_combine_with_and_semantics() {
        // BestEffort (from spec.scopes) AND PriorityClass=high (from
        // spec.scopeSelector) -- real upstream's own
        // getScopeSelectorsFromQuota concatenation, both must match.
        let mut q = quota("combo-quota", json!({"pods": "0"}));
        q["spec"]["scopes"] = json!(["BestEffort"]);
        q["spec"]["scopeSelector"] = json!({"matchExpressions": [{"scopeName": "PriorityClass", "operator": "In", "values": ["high"]}]});

        // BestEffort but wrong priority class: scopeSelector fails.
        let mut besteffort_wrong_priority = json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1"}]}});
        besteffort_wrong_priority["spec"]["priorityClassName"] = json!("low");
        assert!(check_pod_create(&besteffort_wrong_priority, &[], &[q.clone()]).is_none());

        // Not BestEffort (has cpu requests) even with the right priority
        // class: spec.scopes fails.
        let mut burstable_right_priority = pod_with_cpu_request("new", "1m");
        burstable_right_priority["spec"]["priorityClassName"] = json!("high");
        assert!(check_pod_create(&burstable_right_priority, &[], &[q.clone()]).is_none());

        // Both match.
        let mut both_match = json!({"metadata": {"name": "new"}, "spec": {"containers": [{"name": "c1"}]}});
        both_match["spec"]["priorityClassName"] = json!("high");
        assert!(check_pod_create(&both_match, &[], &[q]).is_some());
    }

    #[test]
    fn a_scope_selector_alone_makes_the_pvc_evaluator_treat_the_quota_as_scoped() {
        let pvc = pvc_with_storage("new", "999Gi");
        let mut q = quota("scoped-quota", json!({"requests.storage": "1Gi"}));
        q["spec"]["scopeSelector"] = json!({"matchExpressions": [{"scopeName": "PriorityClass", "operator": "Exists"}]});
        assert!(check_pvc_create(&pvc, &[], &[q]).is_none(), "a spec.scopeSelector alone (no spec.scopes) must also make the PVC evaluator skip this quota");
    }
}
