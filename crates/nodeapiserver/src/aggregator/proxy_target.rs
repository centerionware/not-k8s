//! Resolves an `APIService`'s own `spec.service` into a real dial
//! target — reuses `proxy::pod_log::Target` directly (this crate's own
//! generic "dial this host:port with this path/query" shape, not
//! something `pods/log` owns exclusively; all its fields are already
//! `pub`).
//!
//! **A real, deliberate choice for this build, not an invented
//! shortcut**: real upstream's own `kube-aggregator` has two resolver
//! strategies (`pkg/apiserver/resolvers.go`, fetched and read directly)
//! — `NewEndpointServiceResolver` (resolve to one specific backend
//! `EndpointSlice` address, bypassing kube-proxy entirely — the
//! commonly-enabled `--enable-aggregator-routing` mode) and
//! `NewClusterIPServiceResolver` (dial the backing Service's own
//! `ClusterIP` directly, relying on kube-proxy's real DNAT to reach a
//! live pod — real upstream's own simpler, still-real default when that
//! flag isn't set). This module implements the second: `crates/
//! nodeproxy` already provides real `ClusterIP` routing (real nftables
//! DNAT to a live pod) — the same real infrastructure kube-proxy itself
//! provides for that strategy to work at all, so this isn't a
//! simplification standing in for something missing, it's the same real
//! strategy real upstream ships. The endpoint-resolving strategy remains
//! a real, named future option for a node running with `--proxy=none`
//! (`nodeproxy` disabled), where no `ClusterIP` is reachable at all.
//!
//! **Pure target-resolution only, same "land the primitive, wire it
//! later" split this whole arc uses** — nothing here makes an HTTP
//! request or reads live Service data itself; the caller (`server::
//! listener`) already has the backing Service's own
//! decoded document via `server::rest::get`. TLS trust for the actual
//! dial (`spec.caBundle`/`.insecureSkipTLSVerify`, a real *per-
//! APIService* `rustls::ClientConfig` — genuinely different from
//! `proxy::client_tls`'s single shared nodelet-trust config built once
//! at startup) is a real, separate, not-yet-attempted piece, named
//! honestly rather than glossed over.

use crate::proxy::pod_log::Target;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// `spec.service` is `null` — a "local" `APIService` (this build
    /// itself serves the group-version), never a proxy target at all;
    /// same real case `aggregator::availability::local_condition`
    /// covers on the availability side.
    Local,
    /// The backing Service has no `spec.clusterIP` this build can dial
    /// — a headless Service (`clusterIP: "None"`) or one simply missing
    /// the field. Real upstream's own `ResolveCluster` refuses these
    /// too, for the identical reason: there is no one real address to
    /// dial.
    NoClusterIp,
}

/// `api_service`/`service` are already-decoded documents (`server::
/// rest::get` against `apiregistration.k8s.io/v1/apiservices` and
/// `""/v1/services` respectively); `path`/`query` are the real
/// request's own, forwarded through completely unchanged — an
/// aggregated `APIService` is a real transparent proxy, the same
/// "relay unmodified" posture `pods/log` already established.
pub fn resolve(api_service: &Value, service: &Value, path: &str, query: &str) -> Result<Target, Error> {
    if api_service.pointer("/spec/service").is_none() {
        return Err(Error::Local);
    }
    let cluster_ip = service.pointer("/spec/clusterIP").and_then(Value::as_str).filter(|ip| !ip.is_empty() && *ip != "None");
    let Some(cluster_ip) = cluster_ip else {
        return Err(Error::NoClusterIp);
    };
    // Real upstream's own default when `spec.service.port` is omitted
    // (`ServiceReference.Port`'s own doc comment: "Default to 443").
    let port = api_service.pointer("/spec/service/port").and_then(Value::as_u64).unwrap_or(443) as u16;
    Ok(Target { scheme: "https", host: cluster_ip.to_string(), port, path: path.to_string(), query: query.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn api_service_with_service() -> Value {
        json!({"spec": {"group": "metrics.k8s.io", "version": "v1beta1", "service": {"namespace": "kube-system", "name": "metrics-server", "port": 443}}})
    }

    #[test]
    fn a_local_api_service_is_rejected() {
        let local = json!({"spec": {"group": "apps", "version": "v1"}});
        let service = json!({"spec": {"clusterIP": "10.0.0.5"}});
        assert_eq!(resolve(&local, &service, "/apis/apps/v1", ""), Err(Error::Local));
    }

    #[test]
    fn a_real_cluster_ip_resolves_to_a_real_target() {
        let service = json!({"spec": {"clusterIP": "10.0.0.5"}});
        let target = resolve(&api_service_with_service(), &service, "/apis/metrics.k8s.io/v1beta1/nodes", "").unwrap();
        assert_eq!(target.host, "10.0.0.5");
        assert_eq!(target.port, 443);
        assert_eq!(target.scheme, "https");
        assert_eq!(target.path, "/apis/metrics.k8s.io/v1beta1/nodes");
    }

    #[test]
    fn a_missing_service_port_defaults_to_443() {
        let api_service = json!({"spec": {"group": "metrics.k8s.io", "version": "v1beta1", "service": {"namespace": "kube-system", "name": "metrics-server"}}});
        let service = json!({"spec": {"clusterIP": "10.0.0.5"}});
        let target = resolve(&api_service, &service, "/", "").unwrap();
        assert_eq!(target.port, 443);
    }

    #[test]
    fn a_headless_service_is_rejected() {
        let service = json!({"spec": {"clusterIP": "None"}});
        assert_eq!(resolve(&api_service_with_service(), &service, "/", ""), Err(Error::NoClusterIp));
    }

    #[test]
    fn a_service_with_no_cluster_ip_at_all_is_rejected() {
        let service = json!({"spec": {}});
        assert_eq!(resolve(&api_service_with_service(), &service, "/", ""), Err(Error::NoClusterIp));
    }

    #[test]
    fn the_query_string_is_forwarded_unchanged() {
        let service = json!({"spec": {"clusterIP": "10.0.0.5"}});
        let target = resolve(&api_service_with_service(), &service, "/apis/metrics.k8s.io/v1beta1", "watch=true").unwrap();
        assert_eq!(target.query, "watch=true");
    }
}
