//! Group L Phase 2's live loop: periodically
//! re-checks every stored, non-local `APIService`'s backing Service/
//! `EndpointSlice`/discovery-endpoint health and writes the resulting
//! `Available` condition to `status.conditions` — real upstream's own
//! `local`/`remote` availability controllers (`availability`'s own pure
//! decision logic), now actually run and persisted, not just computed
//! and discarded.
//!
//! **Named, honest scope**: this loop's own condition isn't consulted by
//! `server::listener::aggregate_proxy`/`aggregator::route::
//! discoverable_group_versions` yet — both still run the exact same
//! pre-flight check fresh on every call, a deliberate, separate
//! follow-up (switching the hot path over to read an already-computed
//! condition instead of recomputing it). This slice's real value is
//! populating `status.conditions` for the first time at all — a real,
//! visible `kubectl get apiservice` diagnostic real upstream's own users
//! rely on, previously always empty in this build.
//!
//! The discovery-endpoint dial (`remote`'s own final real check once
//! pre-flight passes) is itself a **named, honest simplification**: real
//! upstream fires 5 concurrent probes, any one succeeding is enough
//! (`Reason: "Passed"`); this does one real dial via the already-proven
//! `proxy::http_client::fetch` against `/apis/{group}/{version}` —
//! concurrency there only ever buys resilience against one flaky
//! backend replica among several, not a materially different real
//! outcome for the common single/few-replica case this build targets.

use crate::aggregator::{availability, client_tls, proxy_target};
use crate::proxy::http_client;
use crate::server::rest;
use crate::storage::client::StorageClient;
use serde_json::{json, Value};

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// One reconciliation pass: lists every stored `APIService`, computes
/// its real `Available` condition ([`compute_condition`]), and writes it
/// back via [`rest::update_status`]. Returns how many were successfully
/// written — a per-object I/O failure is logged and skipped rather than
/// aborting the whole pass, so one unreachable backend doesn't stop
/// every other registered `APIService` from being reconciled.
pub async fn reconcile_once(storage: &mut StorageClient) -> Result<usize, rest::Error> {
    let list = match rest::list(storage, None, "apiregistration.k8s.io", "v1", "apiservices", None, "", "", 0, "").await? {
        rest::ListOutcome::Found(list) => list,
        rest::ListOutcome::UnknownResource | rest::ListOutcome::InvalidContinueToken => return Ok(0),
    };
    let items: Vec<Value> = list.get("items").and_then(Value::as_array).cloned().unwrap_or_default();

    let mut reconciled = 0;
    for mut item in items {
        let name = item.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let condition = compute_condition(storage, &item).await;
        item["status"] = json!({ "conditions": [condition_to_json(&condition)] });
        match rest::update_status(storage, "apiregistration.k8s.io", "v1", "apiservices", None, &name, &item, false).await {
            Ok(rest::UpdateOutcome::Updated(_)) => reconciled += 1,
            Ok(other) => {
                tracing::warn!(name = %name, outcome = ?other, "aggregator::reconcile: writing the Available condition was not accepted");
            }
            Err(e) => {
                tracing::warn!(name = %name, error = ?e, "aggregator::reconcile: writing the Available condition failed");
            }
        }
    }
    Ok(reconciled)
}

/// Real upstream's own `local`/`remote` decision (`availability`'s own
/// doc comment): `spec.service: null` is always `Available` (`"Local"`);
/// otherwise runs the real pre-flight chain, and — only once that
/// passes — the discovery-endpoint dial this module's own doc comment
/// names as a real, bounded simplification of upstream's 5-concurrent-
/// probe check.
async fn compute_condition(storage: &mut StorageClient, api_service: &Value) -> availability::Condition {
    let Some(service_ref) = api_service.pointer("/spec/service") else {
        return availability::local_condition();
    };
    let namespace = service_ref.get("namespace").and_then(Value::as_str).unwrap_or("").to_string();
    let name = service_ref.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let port = service_ref.get("port").and_then(Value::as_i64).unwrap_or(443);

    let service = match rest::get(storage, None, "", "v1", "services", Some(&namespace), &name).await {
        Ok(rest::GetOutcome::Found(object)) => Some(object),
        _ => None,
    };
    let endpoint_slices = match rest::list(storage, None, "discovery.k8s.io", "v1", "endpointslices", Some(&namespace), &format!("kubernetes.io/service-name={name}"), "", 0, "").await {
        Ok(rest::ListOutcome::Found(list)) => list.get("items").and_then(Value::as_array).cloned().unwrap_or_default(),
        _ => Vec::new(),
    };

    if let Err(condition) = availability::preflight_check(&namespace, &name, port, service.as_ref(), &endpoint_slices) {
        return condition;
    }
    let Some(service) = service else {
        // preflight_check itself never returns Ok(()) with service: None
        // (a missing service is always a real ServiceNotFound Err) --
        // kept real rather than unreachable!() since that contract lives
        // in a sibling module, not enforced by the type system here.
        return availability::Condition { status: availability::ConditionStatus::False, reason: "ServiceNotFound", message: format!("service/{name} in {namespace:?} is not present") };
    };

    let group = api_service.pointer("/spec/group").and_then(Value::as_str).unwrap_or("");
    let version = api_service.pointer("/spec/version").and_then(Value::as_str).unwrap_or("");
    let target = match proxy_target::resolve(api_service, &service, &format!("/apis/{group}/{version}"), "") {
        Ok(t) => t,
        Err(_) => {
            return availability::Condition {
                status: availability::ConditionStatus::False,
                reason: "FailedDiscoveryCheck",
                message: "could not resolve a dial target for the backing service".to_string(),
            };
        }
    };
    let insecure_skip_tls_verify = api_service.pointer("/spec/insecureSkipTLSVerify").and_then(Value::as_bool).unwrap_or(false);
    let ca_bundle_pem = api_service.pointer("/spec/caBundle").and_then(Value::as_str).filter(|b| !b.is_empty()).and_then(|b64| {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(b64).ok()
    });
    let client_config = match client_tls::build_client_config(ca_bundle_pem.as_deref(), insecure_skip_tls_verify) {
        Ok(cfg) => std::sync::Arc::new(cfg),
        Err(_) => {
            return availability::Condition {
                status: availability::ConditionStatus::False,
                reason: "FailedDiscoveryCheck",
                message: "could not build a TLS client config for the backing service".to_string(),
            };
        }
    };
    match http_client::fetch(&target, client_config).await {
        Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {
            availability::Condition { status: availability::ConditionStatus::True, reason: "Passed", message: "all checks passed".to_string() }
        }
        Ok(resp) => availability::Condition {
            status: availability::ConditionStatus::False,
            reason: "FailedDiscoveryCheck",
            message: format!("failing or missing response from discovery check: got status {}", resp.status()),
        },
        Err(e) => availability::Condition { status: availability::ConditionStatus::False, reason: "FailedDiscoveryCheck", message: format!("failing or missing response from discovery check: {e}") },
    }
}

/// Real upstream's own `APIServiceCondition` shape
/// (`pkg/apis/apiregistration/v1/types.go`) — `Type` is always
/// `"Available"` for this controller, `LastTransitionTime` stamped fresh
/// on every pass (this build keeps no separate prior-condition state to
/// diff against, a named simplification: real upstream only updates the
/// timestamp when `Status` actually changes).
fn condition_to_json(c: &availability::Condition) -> Value {
    json!({
        "type": "Available",
        "status": c.status.as_str(),
        "reason": c.reason,
        "message": c.message,
        "lastTransitionTime": now_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_to_json_has_the_real_apiservicecondition_shape() {
        let c = availability::local_condition();
        let doc = condition_to_json(&c);
        assert_eq!(doc["type"], "Available");
        assert_eq!(doc["status"], "True");
        assert_eq!(doc["reason"], "Local");
        assert!(doc["lastTransitionTime"].as_str().unwrap().ends_with('Z'));
    }
}
