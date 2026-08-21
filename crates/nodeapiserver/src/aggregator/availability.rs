//! The availability controller's pure decision logic — a faithful port
//! of real upstream's own `pkg/controllers/status/{local,remote}/
//! *_available_controller.go` (`github.com/kubernetes/kube-aggregator`,
//! fetched and read directly, both files — a genuinely separate GitHub
//! repo from `kubernetes/kubernetes`, not a staging package this time).
//!
//! Two real, separate controllers upstream: `local` (a `spec.service:
//! null` `APIService` — this build serves the group-version itself) is
//! *always* `Available`, unconditionally
//! ([`local_condition`]/`NewLocalAvailableAPIServiceCondition`). `remote`
//! runs a real chain of pre-flight checks *before* ever attempting the
//! actual discovery-endpoint dial: does the backing Service exist, is it
//! listening on the configured port (only checked for a `ClusterIP`
//! service — any other type skips straight to the dial), does it have
//! any ready `EndpointSlice` addresses on that port. Each failure mode
//! has its own real `Reason` string upstream itself uses
//! (`ServiceNotFound`/`ServicePortError`/`EndpointsNotFound`/
//! `MissingEndpoints`), matched here exactly rather than invented.
//!
//! **[`preflight_check`] is pure — no I/O of its own**: `service`/
//! `endpoint_slices` are already-fetched `serde_json::Value` documents,
//! which this crate's own Group D watch cache already has live for any
//! real Service/EndpointSlice. The actual discovery-endpoint dial (real
//! upstream's own "5 concurrent `GET /apis/<group>/<version>` attempts,
//! any one succeeding is enough" check, `Reason: "FailedDiscoveryCheck"`
//! on failure / `"Passed"` on success) is Group L's own Phase 4 — the
//! same real `proxy::http_client`/`proxy::client_tls` dial-and-relay
//! primitives Group N already proved out for `pods/log` are the natural
//! fit, not attempted in this module (`docs/APISERVER.md`'s own Group L
//! section covers why). A caller that gets `Ok(())` from this function
//! should go on to attempt that dial; `Err(Condition)` is the real,
//! final `Available` condition upstream itself would have set without
//! ever attempting one.

use serde_json::Value;

/// The three real `ConditionStatus` values real upstream's own
/// `apiregistrationv1.ConditionStatus` — a plain string enum, same
/// convention `metav1.Condition`/every other Kubernetes condition type
/// uses — never anything but one of these three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionStatus {
    True,
    False,
    Unknown,
}

impl ConditionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ConditionStatus::True => "True",
            ConditionStatus::False => "False",
            ConditionStatus::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub status: ConditionStatus,
    pub reason: &'static str,
    pub message: String,
}

fn condition(status: ConditionStatus, reason: &'static str, message: String) -> Condition {
    Condition { status, reason, message }
}

/// Real upstream's own `NewLocalAvailableAPIServiceCondition` — exact
/// `Reason`/`Message` strings, confirmed directly against
/// `pkg/apis/apiregistration/v1/helper/helpers.go`.
pub fn local_condition() -> Condition {
    condition(ConditionStatus::True, "Local", "Local APIServices are always available".to_string())
}

/// The real pre-flight chain `remote_available_controller.go` runs
/// before its own discovery-endpoint dial — see this module's own doc
/// comment for the full real behavior and what's deliberately not
/// attempted here yet. `service: None` matches upstream's own "lister
/// returned `NotFound`" case; `endpoint_slices` empty matches "lister
/// found none" — both real, not-found outcomes this crate's own watch
/// cache can produce identically to a real lister miss.
pub fn preflight_check(namespace: &str, name: &str, port: i64, service: Option<&Value>, endpoint_slices: &[Value]) -> Result<(), Condition> {
    let Some(service) = service else {
        return Err(condition(ConditionStatus::False, "ServiceNotFound", format!("service/{name} in {namespace:?} is not present")));
    };

    // Real upstream only runs the port/endpoint check for a ClusterIP
    // service (`v1.ServiceTypeClusterIP`, real upstream's own default
    // when `spec.type` is omitted) — any other type (`ExternalName`,
    // ...) skips straight to the discovery dial.
    if service.pointer("/spec/type").and_then(Value::as_str).unwrap_or("ClusterIP") != "ClusterIP" {
        return Ok(());
    }

    let port_name = service
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|p| p.get("port").and_then(Value::as_i64) == Some(port))
        .map(|p| p.get("name").and_then(Value::as_str).unwrap_or("").to_string());
    let Some(port_name) = port_name else {
        return Err(condition(ConditionStatus::False, "ServicePortError", format!("service/{name} in {namespace:?} is not listening on port {port}")));
    };

    if endpoint_slices.is_empty() {
        return Err(condition(ConditionStatus::False, "EndpointsNotFound", format!("cannot find endpointslices for service/{name} in {namespace:?}")));
    }

    let has_active_endpoints = endpoint_slices.iter().any(|slice| {
        // Real upstream's own default: an endpoint with no `ready`
        // condition set at all counts as ready (`Conditions.Ready ==
        // nil` -> `true`), same as this crate's own generic model of
        // "absent means the field's own real default" elsewhere.
        let any_ready = slice
            .pointer("/endpoints")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|e| e.pointer("/conditions/ready").and_then(Value::as_bool).unwrap_or(true));
        if !any_ready {
            return false;
        }
        slice
            .pointer("/ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|p| p.get("name").and_then(Value::as_str) == Some(port_name.as_str()) && p.get("port").is_some())
    });
    if !has_active_endpoints {
        return Err(condition(ConditionStatus::False, "MissingEndpoints", format!("endpointslices for service/{name} in {namespace:?} have no addresses with port name {port_name:?}")));
    }

    Ok(())
}

/// Reads back the `Available` condition `aggregator::reconcile::
/// reconcile_once` already wrote to `status.conditions`, if any —
/// `Some(true)`/`Some(false)` for a real, already-computed `True`/
/// `False` status, `None` when no `Available` condition exists yet (a
/// freshly-created `APIService` the reconciliation loop hasn't reached
/// on its first pass) or its `status` is `"Unknown"` (real upstream's
/// own third `ConditionStatus`, never written by this crate's own
/// controller today but handled the same conservative way a caller
/// should treat any condition it can't confidently act on: fall back to
/// computing it fresh).
///
/// Callers use this to skip the real I/O `preflight_check` needs
/// (fetching the backing Service/`EndpointSlice`s) when a fresh,
/// decisive cached answer already exists — never trusted as the *only*
/// signal on a `None` (real correctness would otherwise regress for the
/// first ~30s after a new `APIService` is registered, before the
/// reconciliation loop's first pass reaches it).
pub fn cached_available(api_service: &Value) -> Option<bool> {
    let conditions = api_service.pointer("/status/conditions").and_then(Value::as_array)?;
    let available = conditions.iter().find(|c| c.get("type").and_then(Value::as_str) == Some("Available"))?;
    match available.get("status").and_then(Value::as_str) {
        Some("True") => Some(true),
        Some("False") => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn local_is_always_available() {
        let c = local_condition();
        assert_eq!(c.status, ConditionStatus::True);
        assert_eq!(c.reason, "Local");
    }

    #[test]
    fn a_missing_service_is_service_not_found() {
        let err = preflight_check("kube-system", "metrics-server", 443, None, &[]).unwrap_err();
        assert_eq!(err.status, ConditionStatus::False);
        assert_eq!(err.reason, "ServiceNotFound");
    }

    fn cluster_ip_service(ports: Value) -> Value {
        json!({"spec": {"type": "ClusterIP", "ports": ports}})
    }

    #[test]
    fn a_service_not_listening_on_the_configured_port_is_a_service_port_error() {
        let service = cluster_ip_service(json!([{"name": "https", "port": 8443}]));
        let err = preflight_check("kube-system", "metrics-server", 443, Some(&service), &[]).unwrap_err();
        assert_eq!(err.reason, "ServicePortError");
    }

    #[test]
    fn no_endpoint_slices_at_all_is_endpoints_not_found() {
        let service = cluster_ip_service(json!([{"name": "https", "port": 443}]));
        let err = preflight_check("kube-system", "metrics-server", 443, Some(&service), &[]).unwrap_err();
        assert_eq!(err.reason, "EndpointsNotFound");
    }

    #[test]
    fn an_endpoint_slice_with_no_ready_address_on_the_right_port_is_missing_endpoints() {
        let service = cluster_ip_service(json!([{"name": "https", "port": 443}]));
        let slices = vec![json!({
            "endpoints": [{"conditions": {"ready": false}}],
            "ports": [{"name": "https", "port": 443}],
        })];
        let err = preflight_check("kube-system", "metrics-server", 443, Some(&service), &slices).unwrap_err();
        assert_eq!(err.reason, "MissingEndpoints");
    }

    #[test]
    fn a_ready_endpoint_on_the_matching_port_passes_preflight() {
        let service = cluster_ip_service(json!([{"name": "https", "port": 443}]));
        let slices = vec![json!({
            "endpoints": [{"conditions": {"ready": true}}],
            "ports": [{"name": "https", "port": 443}],
        })];
        assert_eq!(preflight_check("kube-system", "metrics-server", 443, Some(&service), &slices), Ok(()));
    }

    #[test]
    fn an_endpoint_with_no_ready_condition_at_all_defaults_to_ready() {
        // Real upstream's own default: Conditions.Ready == nil means
        // ready, not not-ready.
        let service = cluster_ip_service(json!([{"name": "https", "port": 443}]));
        let slices = vec![json!({
            "endpoints": [{}],
            "ports": [{"name": "https", "port": 443}],
        })];
        assert_eq!(preflight_check("kube-system", "metrics-server", 443, Some(&service), &slices), Ok(()));
    }

    #[test]
    fn a_non_cluster_ip_service_skips_the_port_and_endpoint_checks_entirely() {
        let service = json!({"spec": {"type": "ExternalName"}});
        assert_eq!(preflight_check("kube-system", "metrics-server", 443, Some(&service), &[]), Ok(()));
    }

    #[test]
    fn a_service_with_no_spec_type_at_all_defaults_to_cluster_ip() {
        let service = json!({"spec": {"ports": []}});
        let err = preflight_check("kube-system", "metrics-server", 443, Some(&service), &[]).unwrap_err();
        assert_eq!(err.reason, "ServicePortError", "an absent spec.type must still run the ClusterIP checks, real upstream's own default");
    }

    #[test]
    fn cached_available_reads_a_true_condition() {
        let api_service = json!({"status": {"conditions": [{"type": "Available", "status": "True"}]}});
        assert_eq!(cached_available(&api_service), Some(true));
    }

    #[test]
    fn cached_available_reads_a_false_condition() {
        let api_service = json!({"status": {"conditions": [{"type": "Available", "status": "False"}]}});
        assert_eq!(cached_available(&api_service), Some(false));
    }

    #[test]
    fn cached_available_is_none_when_no_available_condition_exists_yet() {
        assert_eq!(cached_available(&json!({})), None);
        assert_eq!(cached_available(&json!({"status": {"conditions": []}})), None);
        assert_eq!(cached_available(&json!({"status": {"conditions": [{"type": "SomeOtherType", "status": "True"}]}})), None);
    }

    #[test]
    fn cached_available_is_none_for_an_unknown_status_rather_than_a_guess() {
        let api_service = json!({"status": {"conditions": [{"type": "Available", "status": "Unknown"}]}});
        assert_eq!(cached_available(&api_service), None);
    }
}
