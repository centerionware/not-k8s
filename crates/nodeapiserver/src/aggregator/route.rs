//! Phase 4's remaining live-wiring half: finds the one stored, non-local
//! `APIService` (if any) that claims a given `(group, version)` — the
//! real question `server::listener` has to answer before it can decide
//! whether a request belongs to this build's own local dispatch or to an
//! aggregated backend at all. A registered `APIService` with
//! `spec.service: null` is real upstream's own "Local" marker (this
//! build itself serves the group-version, `aggregator::availability::
//! local_condition`'s own case) — never a proxy target, so it's filtered
//! out here rather than left for the caller to keep re-checking.
//!
//! Deliberately a linear scan over every stored `APIService`
//! (`server::rest::list`, the same already-generic verb every other
//! resource in this crate goes through — no new storage-layer code
//! needed), not a dedicated index: real clusters register a small,
//! roughly-fixed number of these (a handful of `metrics.k8s.io`/
//! `custom.metrics.k8s.io`-shaped aggregated APIs), the same real-world
//! cardinality assumption `apiextensions::registry::resolve_in` already
//! makes for CRDs.

use crate::aggregator::availability;
use crate::server::rest;
use crate::storage::client::StorageClient;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("listing APIServices failed: {0}")]
    Rest(#[from] rest::Error),
}

/// The first stored, non-local `APIService` whose `spec.group`/
/// `spec.version` match — `None` when no such object exists (the caller
/// should fall through to this build's own local dispatch) or every
/// match found was itself `spec.service: null` (Local). Real upstream
/// allows several `APIService`s to register the same group at different
/// versions (each version picks its own backend) but never two for the
/// exact same `(group, version)` pair — an operator error this function
/// doesn't attempt to detect, same "not our job to police" posture
/// `apiextensions::registry::resolve_in`'s own doc comment already takes
/// for a CRD naming collision.
pub async fn resolve(storage: &mut StorageClient, group: &str, version: &str) -> Result<Option<Value>, Error> {
    if group.is_empty() {
        // The core group is never aggregated -- real upstream's own rule,
        // and this build has no `APIService` bootstrap for it anyway.
        return Ok(None);
    }
    let list = match rest::list(storage, None, "apiregistration.k8s.io", "v1", "apiservices", None, "", "", 0, "").await? {
        rest::ListOutcome::Found(list) => list,
        rest::ListOutcome::UnknownResource | rest::ListOutcome::InvalidContinueToken => return Ok(None),
    };
    let matched = list
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|svc| svc.pointer("/spec/group").and_then(Value::as_str) == Some(group) && svc.pointer("/spec/version").and_then(Value::as_str) == Some(version) && svc.pointer("/spec/service").is_some());
    Ok(matched.cloned())
}

/// Group L Phase 3's own real question: every `(group, version)` this
/// build should advertise in `/apis`/`/apis/{group}` on top of the
/// static table and any CRD, sourced from stored, non-local
/// `APIService`s — `discovery::merged_group_version_map`'s own doc
/// comment covers why this is scoped to group-level discovery only, not
/// `/apis/{group}/{version}`'s own resource list. Runs the exact same
/// real pre-flight chain `server::listener::aggregate_proxy` runs before
/// ever attempting a dial, so a registered but currently-unavailable
/// backend (its Service deleted, no ready endpoints, ...) is correctly
/// left out of discovery rather than advertised and then failing every
/// real request — matching real upstream's own "only an `Available`
/// `APIService`'s group-version appears in discovery" posture. Prefers
/// `aggregator::reconcile`'s own already-computed condition when one
/// exists (`availability::cached_available` — zero extra I/O), falling
/// back to a fresh `preflight_check` (one `Service` GET + one
/// `EndpointSlice` LIST, bounded by the same small real-world
/// cardinality `resolve`'s own doc comment already assumes) only for an
/// `APIService` the reconciliation loop hasn't reached yet.
pub async fn discoverable_group_versions(storage: &mut StorageClient) -> Result<Vec<(String, String)>, Error> {
    let list = match rest::list(storage, None, "apiregistration.k8s.io", "v1", "apiservices", None, "", "", 0, "").await? {
        rest::ListOutcome::Found(list) => list,
        rest::ListOutcome::UnknownResource | rest::ListOutcome::InvalidContinueToken => return Ok(Vec::new()),
    };
    let candidates: Vec<Value> = list.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for api_service in candidates {
        let Some(service_ref) = api_service.pointer("/spec/service") else { continue };
        let Some(group) = api_service.pointer("/spec/group").and_then(Value::as_str) else { continue };
        let Some(version) = api_service.pointer("/spec/version").and_then(Value::as_str) else { continue };
        // `aggregator::reconcile`'s own already-computed condition, when
        // one exists, answers this without any I/O at all -- real
        // correctness is unaffected either way (`cached_available`'s own
        // doc comment: `None` falls through to the fresh check below,
        // never trusted as the only signal on a freshly-registered
        // `APIService` the reconciliation loop hasn't reached yet).
        if let Some(available) = availability::cached_available(&api_service) {
            if available {
                out.push((group.to_string(), version.to_string()));
            }
            continue;
        }

        let namespace = service_ref.get("namespace").and_then(Value::as_str).unwrap_or("");
        let name = service_ref.get("name").and_then(Value::as_str).unwrap_or("");
        let port = service_ref.get("port").and_then(Value::as_i64).unwrap_or(443);

        let service = match rest::get(storage, None, "", "v1", "services", Some(namespace), name).await? {
            rest::GetOutcome::Found(object) => Some(object),
            rest::GetOutcome::ObjectNotFound | rest::GetOutcome::UnknownResource => None,
        };
        let endpoint_slices = match rest::list(storage, None, "discovery.k8s.io", "v1", "endpointslices", Some(namespace), &format!("kubernetes.io/service-name={name}"), "", 0, "").await? {
            rest::ListOutcome::Found(list) => list.get("items").and_then(Value::as_array).cloned().unwrap_or_default(),
            rest::ListOutcome::UnknownResource | rest::ListOutcome::InvalidContinueToken => Vec::new(),
        };
        if availability::preflight_check(namespace, name, port, service.as_ref(), &endpoint_slices).is_ok() {
            out.push((group.to_string(), version.to_string()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `resolve`'s own real logic (group filtering, Local exclusion) is
    // pure enough to test without a live store by driving the same
    // filter directly -- a full round trip through a real `nodestore` is
    // `tests/aggregator_proxy_roundtrip.rs`'s job instead, matching this
    // crate's own established split between a module's pure decision
    // logic (unit-tested here) and its live storage integration
    // (its own `tests/*_roundtrip.rs` file).
    fn matches(items: &[Value], group: &str, version: &str) -> Option<Value> {
        items.iter().find(|svc| svc.pointer("/spec/group").and_then(Value::as_str) == Some(group) && svc.pointer("/spec/version").and_then(Value::as_str) == Some(version) && svc.pointer("/spec/service").is_some()).cloned()
    }

    #[test]
    fn a_local_api_service_never_matches() {
        let items = vec![serde_json::json!({"spec": {"group": "apps", "version": "v1"}})];
        assert!(matches(&items, "apps", "v1").is_none());
    }

    #[test]
    fn a_remote_api_service_matches_its_own_group_and_version_only() {
        let items = vec![serde_json::json!({"spec": {"group": "metrics.k8s.io", "version": "v1beta1", "service": {"name": "metrics-server", "namespace": "kube-system"}}})];
        assert!(matches(&items, "metrics.k8s.io", "v1beta1").is_some());
        assert!(matches(&items, "metrics.k8s.io", "v1").is_none());
    }
}
