//! `LimitRanger` — a faithful-but-scoped port of real upstream's own
//! admission plugin (`plugin/pkg/admission/limitranger/admission.go`,
//! release-1.34, fetched and read directly): enforces (and, for
//! containers, defaults) `resources.requests`/`resources.limits` against
//! every `LimitRange` object in a `Pod`'s or `PersistentVolumeClaim`'s
//! namespace.
//!
//! Ported: **container-level** (`LimitRange.spec.limits[].type ==
//! "Container"`) min/max/ratio enforcement across both `containers` and
//! `initContainers`, container-level defaulting (`MutateLimit` — a
//! container missing a request/limit for a resource the `LimitRange`
//! carries a `default`/`defaultRequest` for gets it filled in, and the
//! pod is annotated `kubernetes.io/limit-ranger` describing what was set
//! — upstream's own real annotation key and message format, ported
//! exactly), **pod-level** (`LimitTypePod`) aggregate min/max/ratio
//! enforcement against the pod-wide summed requests/limits
//! ([`pod_requests`]/[`pod_limits`], real upstream's own
//! restartable-init-container/sidecar aggregation rules ported exactly —
//! see their own doc comments), and **`PersistentVolumeClaim`-level**
//! (`LimitTypePersistentVolumeClaim`) min/max enforcement on
//! `spec.resources.requests` (upstream's own real behavior: PVCs are
//! validated, never defaulted — storage is a required part of the spec).
//!
//! **Not ported, named honestly as separate follow-up work**: the real
//! `PodLevelResources` feature-gate override (pod-level `spec.resources`
//! overriding the aggregated per-container total for CPU/memory — an
//! alpha feature this crate has no feature-gate machinery to model, so
//! `pod_requests`/`pod_limits` always use the aggregated per-container
//! total, never a pod-level override), the `resize` subresource path
//! (this crate serves no subresources yet), and upstream's own
//! live-lookup LRU/singleflight cache for `LimitRange` objects — this
//! plugin always lists live from storage, same posture
//! `namespace_lifecycle` already takes and for the same reason (nothing in
//! this crate's admission path is served from a potentially-stale cache
//! to begin with, so there's no staleness to work around).
//!
//! Comparisons use [`crate::scheme::quantity::Quantity`] directly rather
//! than porting upstream's own `requestLimitEnforcedValues`/
//! `MaxMilliValue` overflow-avoidance dance — that dance exists purely to
//! avoid overflowing `int64` at large magnitudes, a problem this crate's
//! `i128`-backed `Quantity` doesn't have (see that module's own doc
//! comment).
//!
//! Same split as every other Group J plugin: pure decision/mutation
//! functions (unit tested with no I/O) plus the one real I/O step
//! (`server::rest::list` over `LimitRange` in the target namespace)
//! `server::listener` performs in between.

use crate::admission::attributes::Operation;
use crate::scheme::quantity::Quantity;
use serde_json::{json, Value};

const LIMIT_RANGER_ANNOTATION: &str = "kubernetes.io/limit-ranger";

/// Real upstream's own `SupportsAttributes`, minus the `resize`
/// subresource carve-out (not applicable — this crate serves no
/// subresources at all, so the "no other subresources are supported"
/// branch already covers it) and minus in-place-resize's own consequence
/// (`Pod` `UPDATE` is never supported here either way, matching
/// upstream's own "containers/initContainers are immutable after create,
/// so mutating/validating limits on update is unnecessary" reasoning).
pub fn applies_to(operation: Operation, group: &str, resource: &str, subresource: &str) -> bool {
    if !subresource.is_empty() {
        return false;
    }
    if group.is_empty() && resource == "pods" {
        return operation == Operation::Create;
    }
    if group.is_empty() && resource == "persistentvolumeclaims" {
        return matches!(operation, Operation::Create | Operation::Update);
    }
    false
}

fn get_quantity(resource_list: &Value, key: &str) -> Result<Option<Quantity>, String> {
    match resource_list.get(key) {
        None => Ok(None),
        Some(v) => {
            let s = v.as_str().ok_or_else(|| format!("{key} is not a valid quantity"))?;
            Quantity::parse(s).map(Some).map_err(|e| e.to_string())
        }
    }
}

fn min_constraint(limit_type: &str, name: &str, enforced: Quantity, request: &Value, limit: &Value) -> Result<(), String> {
    let req = get_quantity(request, name)?;
    let Some(req) = req else {
        return Err(format!("minimum {name} usage per {limit_type} is {enforced}. No request is specified"));
    };
    if req < enforced {
        return Err(format!("minimum {name} usage per {limit_type} is {enforced}, but request is {req}"));
    }
    if let Some(lim) = get_quantity(limit, name)? {
        if lim < enforced {
            return Err(format!("minimum {name} usage per {limit_type} is {enforced}, but limit is {lim}"));
        }
    }
    Ok(())
}

fn max_constraint(limit_type: &str, name: &str, enforced: Quantity, request: &Value, limit: &Value) -> Result<(), String> {
    let Some(lim) = get_quantity(limit, name)? else {
        return Err(format!("maximum {name} usage per {limit_type} is {enforced}. No limit is specified"));
    };
    if lim > enforced {
        return Err(format!("maximum {name} usage per {limit_type} is {enforced}, but limit is {lim}"));
    }
    if let Some(req) = get_quantity(request, name)? {
        if req > enforced {
            return Err(format!("maximum {name} usage per {limit_type} is {enforced}, but request is {req}"));
        }
    }
    Ok(())
}

/// Real upstream's own `maxRequestConstraint` — the max-only variant used
/// where a `limit` map isn't a meaningful concept (PVC storage requests).
fn max_request_constraint(limit_type: &str, name: &str, enforced: Quantity, request: &Value) -> Result<(), String> {
    let Some(req) = get_quantity(request, name)? else {
        return Err(format!("maximum {name} usage per {limit_type} is {enforced}. No request is specified"));
    };
    if req > enforced {
        return Err(format!("maximum {name} usage per {limit_type} is {enforced}, but request is {req}"));
    }
    Ok(())
}

fn ratio_constraint(limit_type: &str, name: &str, enforced: Quantity, request: &Value, limit: &Value) -> Result<(), String> {
    let req = get_quantity(request, name)?;
    let lim = get_quantity(limit, name)?;
    let req_milli = req.map(|q| q.milli_value()).unwrap_or(0);
    let lim_milli = lim.map(|q| q.milli_value()).unwrap_or(0);
    if req.is_none() || req_milli == 0 {
        return Err(format!("{name} max limit to request ratio per {limit_type} is {enforced}, but no request is specified or request is 0"));
    }
    if lim.is_none() || lim_milli == 0 {
        return Err(format!("{name} max limit to request ratio per {limit_type} is {enforced}, but no limit is specified or limit is 0"));
    }
    // Both scaled the same way (milli), so the ratio is exact regardless
    // of which consistent scale is used — no overflow-avoidance dance
    // needed (see this module's own doc comment).
    let ratio = lim_milli as f64 / req_milli as f64;
    let max_ratio = enforced.milli_value() as f64 / 1000.0;
    if ratio > max_ratio {
        return Err(format!("{name} max limit to request ratio per {limit_type} is {enforced}, but provided ratio is {ratio}"));
    }
    Ok(())
}

fn empty_object() -> Value {
    json!({})
}

/// Runs every `min`/`max`/`maxLimitRequestRatio` entry of one
/// `LimitRange.spec.limits[]` item (`limit_type` already filtered to
/// `"Container"` by the caller) against one container's own
/// `resources.requests`/`resources.limits` — real upstream's own
/// `PodValidateLimitFunc` inner loop body, factored out so it runs
/// identically for `containers` and `initContainers`.
fn validate_container_against_limit(limit: &Value, requests: &Value, limits: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let limit_type = limit.get("type").and_then(Value::as_str).unwrap_or("");
    for (kind, constraint) in [("min", 0), ("max", 1), ("maxLimitRequestRatio", 2)] {
        let Some(entries) = limit.get(kind).and_then(Value::as_object) else { continue };
        for (name, enforced_raw) in entries {
            let Some(enforced_str) = enforced_raw.as_str() else { continue };
            let Ok(enforced) = Quantity::parse(enforced_str) else { continue };
            let result = match constraint {
                0 => min_constraint(limit_type, name, enforced, requests, limits),
                1 => max_constraint(limit_type, name, enforced, requests, limits),
                _ => ratio_constraint(limit_type, name, enforced, requests, limits),
            };
            if let Err(e) = result {
                errs.push(e);
            }
        }
    }
    errs
}

type ResourceMap = std::collections::BTreeMap<String, Quantity>;

/// Real upstream's own `addResourceList`: adds every entry of a real
/// `resources.requests`/`resources.limits` JSON object into `acc`,
/// keyed by resource name, summing with any existing entry.
fn add_from_value(acc: &mut ResourceMap, list: &Value) {
    let Some(obj) = list.as_object() else { return };
    for (name, raw) in obj {
        let Some(s) = raw.as_str() else { continue };
        let Ok(q) = Quantity::parse(s) else { continue };
        acc.entry(name.clone()).and_modify(|existing| *existing = *existing + q).or_insert(q);
    }
}

/// Real upstream's own `addResourceList` used map-to-map (its own two
/// call sites both pass a `ResourceList`, which a `ResourceMap` already
/// is here — same operation, just not reading from raw JSON).
fn add_map(acc: &mut ResourceMap, other: &ResourceMap) {
    for (name, q) in other {
        acc.entry(name.clone()).and_modify(|existing| *existing = *existing + *q).or_insert(*q);
    }
}

/// Real upstream's own `maxResourceList`: sets `acc[name]` to the
/// greater of its current value (if any) and `other[name]`, for every
/// resource in `other`.
fn max_map(acc: &mut ResourceMap, other: &ResourceMap) {
    for (name, q) in other {
        acc.entry(name.clone()).and_modify(|existing| *existing = (*existing).max(*q)).or_insert(*q);
    }
}

fn resource_value<'a>(container: &'a Value, key: &str, empty: &'a Value) -> &'a Value {
    container.get("resources").and_then(|r| r.get(key)).unwrap_or(empty)
}

/// Real upstream's own `podRequests`/`podLimits`, ported exactly (the
/// same algorithm handles both — `field` selects `"requests"` or
/// `"limits"`): sums every ordinary container's own value, then folds in
/// init containers in declaration order — a *restartable* init container
/// (`restartPolicy: Always`, a real "sidecar" that keeps running
/// alongside the main containers) adds its own value cumulatively into
/// both the pod-wide total *and* a running sidecar subtotal (since it's
/// still consuming resources once the main containers start); an
/// *ordinary* init container's own value is compared against `own value
/// + the running sidecar subtotal so far` (it needs at least that much
/// headroom while it runs, but doesn't itself add to the steady-state
/// total once it exits) — and only the *maximum* such requirement across
/// every init container ends up folded into the pod-wide total, since
/// ordinary init containers run sequentially, never concurrently with
/// each other.
fn pod_resource_map(pod: &Value, field: &str) -> ResourceMap {
    let empty = empty_object();
    let mut total = ResourceMap::new();
    for container in pod.get("spec").and_then(|s| s.get("containers")).and_then(Value::as_array).into_iter().flatten() {
        add_from_value(&mut total, resource_value(container, field, &empty));
    }

    let mut restartable_init_total = ResourceMap::new();
    let mut init_total = ResourceMap::new();
    for container in pod.get("spec").and_then(|s| s.get("initContainers")).and_then(Value::as_array).into_iter().flatten() {
        let is_sidecar = container.get("restartPolicy").and_then(Value::as_str) == Some("Always");
        let this_container = if is_sidecar {
            let mut own = ResourceMap::new();
            add_from_value(&mut own, resource_value(container, field, &empty));
            add_map(&mut total, &own);
            add_map(&mut restartable_init_total, &own);
            restartable_init_total.clone()
        } else {
            let mut own = ResourceMap::new();
            add_from_value(&mut own, resource_value(container, field, &empty));
            add_map(&mut own, &restartable_init_total);
            own
        };
        max_map(&mut init_total, &this_container);
    }
    max_map(&mut total, &init_total);

    total
}

pub fn pod_requests(pod: &Value) -> ResourceMap {
    pod_resource_map(pod, "requests")
}

pub fn pod_limits(pod: &Value) -> ResourceMap {
    pod_resource_map(pod, "limits")
}

fn resource_map_to_value(map: &ResourceMap) -> Value {
    Value::Object(map.iter().map(|(name, q)| (name.clone(), Value::String(q.to_string()))).collect())
}

/// Real upstream's own `PodValidateLimitFunc`: container-level
/// (`LimitTypeContainer`, across both `containers` and `initContainers`)
/// and pod-level (`LimitTypePod`, against [`pod_requests`]/
/// [`pod_limits`]'s aggregated totals) enforcement. `limit_range` is one
/// `LimitRange` object's full JSON value.
pub fn validate_pod(limit_range: &Value, pod: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let empty = empty_object();
    let Some(limits) = limit_range.get("spec").and_then(|s| s.get("limits")).and_then(Value::as_array) else {
        return errs;
    };
    for limit in limits {
        match limit.get("type").and_then(Value::as_str) {
            Some("Container") => {
                for key in ["containers", "initContainers"] {
                    let Some(containers) = pod.get("spec").and_then(|s| s.get(key)).and_then(Value::as_array) else { continue };
                    for container in containers {
                        let requests = container.get("resources").and_then(|r| r.get("requests")).unwrap_or(&empty);
                        let limits_map = container.get("resources").and_then(|r| r.get("limits")).unwrap_or(&empty);
                        errs.extend(validate_container_against_limit(limit, requests, limits_map));
                    }
                }
            }
            Some("Pod") => {
                let requests = resource_map_to_value(&pod_requests(pod));
                let limits_map = resource_map_to_value(&pod_limits(pod));
                errs.extend(validate_container_against_limit(limit, &requests, &limits_map));
            }
            _ => {}
        }
    }
    errs
}

/// Real upstream's own `PersistentVolumeClaimValidateLimitFunc`: min/max
/// only (no ratio — a PVC has no `limits` map, only `requests`), applied
/// to `spec.resources.requests`.
pub fn validate_pvc(limit_range: &Value, pvc: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let empty = empty_object();
    let Some(limits) = limit_range.get("spec").and_then(|s| s.get("limits")).and_then(Value::as_array) else {
        return errs;
    };
    let requests = pvc.get("spec").and_then(|s| s.get("resources")).and_then(|r| r.get("requests")).unwrap_or(&empty);
    for limit in limits {
        if limit.get("type").and_then(Value::as_str) != Some("PersistentVolumeClaim") {
            continue;
        }
        let limit_type = limit.get("type").and_then(Value::as_str).unwrap_or("");
        if let Some(mins) = limit.get("min").and_then(Value::as_object) {
            for (name, enforced_raw) in mins {
                let Some(enforced_str) = enforced_raw.as_str() else { continue };
                let Ok(enforced) = Quantity::parse(enforced_str) else { continue };
                if let Err(e) = min_constraint(limit_type, name, enforced, requests, &empty) {
                    errs.push(e);
                }
            }
        }
        if let Some(maxes) = limit.get("max").and_then(Value::as_object) {
            for (name, enforced_raw) in maxes {
                let Some(enforced_str) = enforced_raw.as_str() else { continue };
                let Ok(enforced) = Quantity::parse(enforced_str) else { continue };
                if let Err(e) = max_request_constraint(limit_type, name, enforced, requests) {
                    errs.push(e);
                }
            }
        }
    }
    errs
}

/// Real upstream's own `defaultContainerResourceRequirements` +
/// `mergeContainerResources`: for every `Container`-type limit's own
/// `default`/`defaultRequest` map, fill in any resource a container
/// doesn't already specify a limit/request for. Returns the container
/// annotation fragments upstream's own `mergeContainerResources`
/// generates (`"<sorted resource names> request for container <name>"`,
/// same for limits) — the caller (`mutate_pod`) joins and annotates.
fn merge_container_resources(container: &mut Value, limit_range: &Value, kind_label: &str) -> Vec<String> {
    let mut set_requests = Vec::new();
    let mut set_limits = Vec::new();

    let container_name = container.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let Some(container_obj) = container.as_object_mut() else { return Vec::new() };
    let resources = container_obj.entry("resources").or_insert_with(|| json!({})).as_object_mut().expect("resources is always an object here");
    // Ensure both maps exist up front; each is then re-borrowed with its
    // own short-lived `get_mut` inside the loop below (never
    // simultaneously — `requests`/`limits` can't both be held mutably at
    // once from the same `resources` map).
    resources.entry("requests").or_insert_with(|| json!({}));
    resources.entry("limits").or_insert_with(|| json!({}));

    let Some(container_limits) = limit_range.get("spec").and_then(|s| s.get("limits")).and_then(Value::as_array) else {
        return Vec::new();
    };
    for limit in container_limits {
        if limit.get("type").and_then(Value::as_str) != Some("Container") {
            continue;
        }
        if let Some(default_request) = limit.get("defaultRequest").and_then(Value::as_object) {
            let requests = resources.get_mut("requests").and_then(Value::as_object_mut).expect("requests was just ensured to exist as an object");
            for (name, value) in default_request {
                if !requests.contains_key(name) {
                    requests.insert(name.clone(), value.clone());
                    set_requests.push(name.clone());
                }
            }
        }
        if let Some(default_limit) = limit.get("default").and_then(Value::as_object) {
            let limits = resources.get_mut("limits").and_then(Value::as_object_mut).expect("limits was just ensured to exist as an object");
            for (name, value) in default_limit {
                if !limits.contains_key(name) {
                    limits.insert(name.clone(), value.clone());
                    set_limits.push(name.clone());
                }
            }
        }
    }

    let mut fragments = Vec::new();
    if !set_requests.is_empty() {
        set_requests.sort();
        fragments.push(format!("{} request for {kind_label} {container_name}", set_requests.join(", ")));
    }
    if !set_limits.is_empty() {
        set_limits.sort();
        fragments.push(format!("{} limit for {kind_label} {container_name}", set_limits.join(", ")));
    }
    fragments
}

/// Real upstream's own `PodMutateLimitFunc`/`mergePodResourceRequirements`:
/// defaults every container's/init container's requests/limits from
/// every `Container`-type `LimitRange` in `limit_ranges`, then annotates
/// the pod with what was set (upstream's own real annotation key and
/// message format, ported exactly: `"LimitRanger plugin set: " +
/// <fragments joined by "; ">"`). No-op (no annotation added) if nothing
/// needed defaulting.
pub fn mutate_pod(pod: &mut Value, limit_ranges: &[Value]) {
    let mut all_fragments = Vec::new();
    for limit_range in limit_ranges {
        let Some(spec) = pod.get_mut("spec") else { continue };
        for key in ["containers", "initContainers"] {
            let label = if key == "containers" { "container" } else { "init container" };
            let Some(containers) = spec.get_mut(key).and_then(Value::as_array_mut) else { continue };
            for container in containers.iter_mut() {
                all_fragments.extend(merge_container_resources(container, limit_range, label));
            }
        }
    }
    if all_fragments.is_empty() {
        return;
    }
    let value = format!("LimitRanger plugin set: {}", all_fragments.join("; "));
    if let Some(metadata) = pod.as_object_mut().and_then(|o| o.entry("metadata").or_insert_with(|| json!({})).as_object_mut()) {
        metadata.entry("annotations").or_insert_with(|| json!({})).as_object_mut().expect("annotations is always an object here").insert(LIMIT_RANGER_ANNOTATION.to_string(), Value::String(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container_limit_range(min: Value, max: Value, ratio: Value) -> Value {
        json!({"spec": {"limits": [{"type": "Container", "min": min, "max": max, "maxLimitRequestRatio": ratio}]}})
    }

    #[test]
    fn applies_to_pods_only_on_create() {
        assert!(applies_to(Operation::Create, "", "pods", ""));
        assert!(!applies_to(Operation::Update, "", "pods", ""));
        assert!(!applies_to(Operation::Delete, "", "pods", ""));
    }

    #[test]
    fn applies_to_pvcs_on_create_and_update() {
        assert!(applies_to(Operation::Create, "", "persistentvolumeclaims", ""));
        assert!(applies_to(Operation::Update, "", "persistentvolumeclaims", ""));
    }

    #[test]
    fn does_not_apply_to_a_subresource_or_another_resource() {
        assert!(!applies_to(Operation::Create, "", "pods", "status"));
        assert!(!applies_to(Operation::Create, "", "deployments", ""));
    }

    #[test]
    fn a_container_below_the_minimum_request_is_rejected() {
        let lr = container_limit_range(json!({"cpu": "100m"}), json!({}), json!({}));
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {"requests": {"cpu": "50m"}}}]}});
        let errs = validate_pod(&lr, &pod);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("minimum cpu"));
    }

    #[test]
    fn a_container_with_no_request_at_all_fails_the_minimum() {
        let lr = container_limit_range(json!({"cpu": "100m"}), json!({}), json!({}));
        let pod = json!({"spec": {"containers": [{"name": "c1"}]}});
        let errs = validate_pod(&lr, &pod);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("No request is specified"));
    }

    #[test]
    fn a_container_within_bounds_passes() {
        let lr = container_limit_range(json!({"cpu": "100m"}), json!({"cpu": "1"}), json!({}));
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {"requests": {"cpu": "200m"}, "limits": {"cpu": "500m"}}}]}});
        assert!(validate_pod(&lr, &pod).is_empty());
    }

    #[test]
    fn a_container_over_the_maximum_limit_is_rejected() {
        let lr = container_limit_range(json!({}), json!({"cpu": "1"}), json!({}));
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {"limits": {"cpu": "2"}}}]}});
        let errs = validate_pod(&lr, &pod);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("maximum cpu"));
    }

    #[test]
    fn a_container_exceeding_the_ratio_is_rejected() {
        let lr = container_limit_range(json!({}), json!({}), json!({"cpu": "2"}));
        let pod = json!({"spec": {"containers": [{"name": "c1", "resources": {"requests": {"cpu": "100m"}, "limits": {"cpu": "300m"}}}]}});
        let errs = validate_pod(&lr, &pod);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("ratio"));
    }

    #[test]
    fn init_containers_are_checked_too() {
        let lr = container_limit_range(json!({"cpu": "100m"}), json!({}), json!({}));
        let pod = json!({"spec": {"initContainers": [{"name": "init1", "resources": {"requests": {"cpu": "10m"}}}]}});
        let errs = validate_pod(&lr, &pod);
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn pod_level_max_is_enforced_against_the_summed_container_total() {
        let lr = json!({"spec": {"limits": [{"type": "Pod", "max": {"cpu": "1"}}]}});
        let pod = json!({"spec": {"containers": [
            {"name": "c1", "resources": {"limits": {"cpu": "600m"}}},
            {"name": "c2", "resources": {"limits": {"cpu": "600m"}}},
        ]}});
        let errs = validate_pod(&lr, &pod);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("maximum cpu"));
    }

    #[test]
    fn pod_level_max_passes_when_the_summed_total_is_within_bounds() {
        let lr = json!({"spec": {"limits": [{"type": "Pod", "max": {"cpu": "1"}}]}});
        let pod = json!({"spec": {"containers": [
            {"name": "c1", "resources": {"limits": {"cpu": "400m"}}},
            {"name": "c2", "resources": {"limits": {"cpu": "400m"}}},
        ]}});
        assert!(validate_pod(&lr, &pod).is_empty());
    }

    #[test]
    fn pod_requests_sums_ordinary_containers() {
        let pod = json!({"spec": {"containers": [
            {"name": "c1", "resources": {"requests": {"cpu": "100m"}}},
            {"name": "c2", "resources": {"requests": {"cpu": "200m"}}},
        ]}});
        assert_eq!(pod_requests(&pod)["cpu"].milli_value(), 300);
    }

    #[test]
    fn pod_requests_adds_a_sidecar_cumulatively() {
        // A restartable init container (restartPolicy: Always) keeps
        // running alongside the main containers, so its own request
        // adds to the pod-wide total, not just a transient init-time max.
        let pod = json!({"spec": {
            "containers": [{"name": "main", "resources": {"requests": {"cpu": "100m"}}}],
            "initContainers": [{"name": "sidecar", "restartPolicy": "Always", "resources": {"requests": {"cpu": "50m"}}}],
        }});
        assert_eq!(pod_requests(&pod)["cpu"].milli_value(), 150);
    }

    #[test]
    fn pod_requests_takes_the_max_of_sequential_init_containers_not_their_sum() {
        // Ordinary (non-restartable) init containers run one at a time,
        // sequentially before the main containers start -- the pod-wide
        // total only needs to cover whichever one needed the most, not
        // all of them added together.
        let pod = json!({"spec": {
            "containers": [{"name": "main", "resources": {"requests": {"cpu": "100m"}}}],
            "initContainers": [
                {"name": "init1", "resources": {"requests": {"cpu": "50m"}}},
                {"name": "init2", "resources": {"requests": {"cpu": "300m"}}},
            ],
        }});
        // max(100m main, 300m largest init) = 300m, not 100m + 50m + 300m.
        assert_eq!(pod_requests(&pod)["cpu"].milli_value(), 300);
    }

    #[test]
    fn pod_requests_folds_a_running_sidecar_total_into_a_sequential_init_containers_own_need() {
        let pod = json!({"spec": {
            "containers": [{"name": "main", "resources": {"requests": {"cpu": "50m"}}}],
            "initContainers": [
                {"name": "sidecar", "restartPolicy": "Always", "resources": {"requests": {"cpu": "100m"}}},
                {"name": "init1", "resources": {"requests": {"cpu": "20m"}}},
            ],
        }});
        // main(50m) + sidecar(100m) = 150m pod-wide so far; init1 needs its
        // own 20m *plus* the already-running sidecar's 100m = 120m while it
        // runs, which is less than 150m, so the pod-wide total stays 150m.
        assert_eq!(pod_requests(&pod)["cpu"].milli_value(), 150);
    }

    #[test]
    fn pvc_below_minimum_storage_is_rejected() {
        let lr = json!({"spec": {"limits": [{"type": "PersistentVolumeClaim", "min": {"storage": "1Gi"}}]}});
        let pvc = json!({"spec": {"resources": {"requests": {"storage": "512Mi"}}}});
        let errs = validate_pvc(&lr, &pvc);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("minimum storage"));
    }

    #[test]
    fn pvc_above_maximum_storage_is_rejected() {
        let lr = json!({"spec": {"limits": [{"type": "PersistentVolumeClaim", "max": {"storage": "10Gi"}}]}});
        let pvc = json!({"spec": {"resources": {"requests": {"storage": "20Gi"}}}});
        let errs = validate_pvc(&lr, &pvc);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("maximum storage"));
    }

    #[test]
    fn pvc_within_bounds_passes() {
        let lr = json!({"spec": {"limits": [{"type": "PersistentVolumeClaim", "min": {"storage": "1Gi"}, "max": {"storage": "10Gi"}}]}});
        let pvc = json!({"spec": {"resources": {"requests": {"storage": "5Gi"}}}});
        assert!(validate_pvc(&lr, &pvc).is_empty());
    }

    #[test]
    fn mutate_pod_fills_in_missing_requests_and_limits_and_annotates() {
        let lr = json!({"spec": {"limits": [{"type": "Container", "defaultRequest": {"cpu": "100m"}, "default": {"cpu": "500m", "memory": "256Mi"}}]}});
        let mut pod = json!({"spec": {"containers": [{"name": "c1"}]}});
        mutate_pod(&mut pod, std::slice::from_ref(&lr));
        assert_eq!(pod["spec"]["containers"][0]["resources"]["requests"]["cpu"], "100m");
        assert_eq!(pod["spec"]["containers"][0]["resources"]["limits"]["cpu"], "500m");
        assert_eq!(pod["spec"]["containers"][0]["resources"]["limits"]["memory"], "256Mi");
        let annotation = pod["metadata"]["annotations"]["kubernetes.io/limit-ranger"].as_str().unwrap();
        assert!(annotation.starts_with("LimitRanger plugin set: "));
        assert!(annotation.contains("request for container c1"));
        assert!(annotation.contains("limit for container c1"));
    }

    #[test]
    fn mutate_pod_does_not_overwrite_an_explicit_value() {
        let lr = json!({"spec": {"limits": [{"type": "Container", "default": {"cpu": "500m"}}]}});
        let mut pod = json!({"spec": {"containers": [{"name": "c1", "resources": {"limits": {"cpu": "1"}}}]}});
        mutate_pod(&mut pod, std::slice::from_ref(&lr));
        assert_eq!(pod["spec"]["containers"][0]["resources"]["limits"]["cpu"], "1");
    }

    #[test]
    fn mutate_pod_is_a_no_op_with_no_limit_ranges() {
        let mut pod = json!({"spec": {"containers": [{"name": "c1"}]}});
        mutate_pod(&mut pod, &[]);
        assert!(pod.get("metadata").is_none());
    }
}
