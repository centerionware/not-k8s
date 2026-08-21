//! `pods/log` proxy target resolution — a faithful port of real upstream's
//! own `pkg/registry/core/pod/strategy.go`'s `LogLocation`/
//! `validateContainer`, plus the node connection-info resolution
//! `pkg/kubelet/client/kubelet_client.go`'s
//! `NodeConnectionInfoGetter.GetConnectionInfo` performs (preferred
//! address-type walk + `daemonEndpoints` port fallback), all fetched and
//! read directly from `kubernetes/kubernetes` at `release-1.34`.
//!
//! **Pure target-resolution only** — the same "land the primitive, wire
//! it later" split this whole arc uses: nothing here makes an HTTP
//! request or talks to nodelet. Wiring this into `server::listener` as a
//! real live proxy needs credential material nodeapiserver can present
//! that nodelet's own bearer-token `TokenReview` authenticator
//! (`crates/nodelet/src/server/auth.rs`) will accept — a real, separate,
//! not-yet-solved problem (this build doesn't implement the
//! `TokenReview` subresource nodelet's authenticator calls back into
//! yet), named honestly as not started rather than glossed over.

use serde_json::Value;

/// Real upstream's own default `--kubelet-port`
/// (`k8s.io/kubernetes/pkg/master/ports`'s `KubeletPort`, mirrored by
/// this crate's sibling `nodelet` as its own `NODELET_SERVER_PORT`
/// default).
pub const DEFAULT_KUBELET_PORT: u16 = 10250;

/// Real upstream's own default `--kubelet-preferred-address-types` order
/// (`cmd/kube-apiserver/app/options/options.go`, fetched and read
/// directly): an operator's `--override-hostname`-driven Hostname first,
/// then DNS before IP within each of the internal/external tiers.
pub const PREFERRED_ADDRESS_TYPES: &[&str] = &["Hostname", "InternalDNS", "InternalIP", "ExternalDNS", "ExternalIP"];

/// The subset of nodelet's own `/containerLogs` query parameters
/// (`crates/nodelet/src/server/logs.rs`) this build forwards — real
/// upstream's `PodLogOptions` also has `sinceSeconds`/`limitBytes`/
/// `stream`, not forwarded here since nodelet's own endpoint doesn't
/// parse them either (nodelet's pre-existing scope, not a new gap this
/// module introduces).
const FORWARDED_QUERY_KEYS: &[&str] = &["follow", "previous", "timestamps", "tailLines", "sinceTime"];

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Real upstream: `"a container name must be specified for pod
    /// <name>, choose one of: [...]"` (or without the suggestion list
    /// when the pod has no containers at all).
    NoDefaultContainer { pod_name: String, candidates: Vec<String> },
    /// Real upstream: `"container <c> is not valid for pod <name>"`.
    UnknownContainer { pod_name: String, container: String },
    /// Real upstream's `LogLocation` returns an empty location (no error)
    /// for a pod with no `spec.nodeName` yet — this build surfaces it as
    /// its own variant instead, since a caller needs to react differently
    /// (a real `400`, most likely) than to a resolved target.
    PodNotScheduled,
    /// The named node has no address of any preferred type at all — real
    /// upstream's own `NoMatchError`.
    NoNodeAddress,
}

fn pod_name(pod: &Value) -> String {
    pod.get("metadata").and_then(|m| m.get("name")).and_then(Value::as_str).unwrap_or("").to_string()
}

fn container_names(pod: &Value) -> Vec<String> {
    let spec = pod.get("spec");
    ["containers", "initContainers"]
        .iter()
        .flat_map(|key| spec.and_then(|s| s.get(key)).and_then(Value::as_array).into_iter().flatten())
        .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// `validateContainer`: resolves the actual container name a log request
/// targets — defaulting when the pod has exactly one container
/// (`containers` + `initContainers` combined, matching real upstream's
/// own `AllFeatureEnabledContainers` visit; `ephemeralContainers` not
/// included, this crate's own established scope elsewhere too, e.g.
/// `admission::service_account`'s own doc comment) and rejecting an
/// explicit name that isn't one of the pod's own containers.
pub fn validate_container(container: &str, pod: &Value) -> Result<String, Error> {
    let names = container_names(pod);
    if container.is_empty() {
        return match names.len() {
            1 => Ok(names.into_iter().next().unwrap()),
            _ => Err(Error::NoDefaultContainer { pod_name: pod_name(pod), candidates: names }),
        };
    }
    if names.iter().any(|n| n == container) {
        Ok(container.to_string())
    } else {
        Err(Error::UnknownContainer { pod_name: pod_name(pod), container: container.to_string() })
    }
}

/// `GetPreferredNodeAddress`: the first `node.status.addresses` entry
/// whose type matches, walking [`PREFERRED_ADDRESS_TYPES`] in order (not
/// the node's own address list order).
pub fn preferred_node_address(node: &Value) -> Option<String> {
    let addresses = node.get("status")?.get("addresses")?.as_array()?;
    for want_type in PREFERRED_ADDRESS_TYPES {
        for addr in addresses {
            if addr.get("type").and_then(Value::as_str) == Some(*want_type) {
                return addr.get("address").and_then(Value::as_str).map(str::to_string);
            }
        }
    }
    None
}

/// `NodeConnectionInfoGetter.GetConnectionInfo`'s own port half: the
/// kubelet-reported port if present and positive, else `default_port`.
pub fn kubelet_port(node: &Value, default_port: u16) -> u16 {
    node.get("status")
        .and_then(|s| s.get("daemonEndpoints"))
        .and_then(|d| d.get("kubeletEndpoint"))
        .and_then(|k| k.get("port"))
        .and_then(Value::as_u64)
        .filter(|&p| p > 0)
        .map(|p| p as u16)
        .unwrap_or(default_port)
}

pub struct Target {
    pub scheme: &'static str,
    pub host: String,
    pub port: u16,
    pub path: String,
    /// Already `key=value&...`-encoded, forwarded keys only (see
    /// [`FORWARDED_QUERY_KEYS`]).
    pub query: String,
}

/// `LogLocation`: the full proxy target for a `pods/log` request —
/// container defaulting/validation, node address/port resolution, and
/// query-param passthrough, ported faithfully except that the pod/node
/// objects are passed in already fetched (this build's caller already has
/// them via `server::rest::get`) rather than fetched internally the way
/// real upstream's own `LogLocation` does.
pub fn log_location(pod: &Value, node: &Value, container: &str, query_pairs: &[(String, String)]) -> Result<Target, Error> {
    let container = validate_container(container, pod)?;

    let node_name = pod.get("spec").and_then(|s| s.get("nodeName")).and_then(Value::as_str).unwrap_or("");
    if node_name.is_empty() {
        return Err(Error::PodNotScheduled);
    }

    let host = preferred_node_address(node).ok_or(Error::NoNodeAddress)?;
    let port = kubelet_port(node, DEFAULT_KUBELET_PORT);

    let namespace = pod.get("metadata").and_then(|m| m.get("namespace")).and_then(Value::as_str).unwrap_or("");
    let name = pod.get("metadata").and_then(|m| m.get("name")).and_then(Value::as_str).unwrap_or("");

    let query = query_pairs
        .iter()
        .filter(|(k, _)| FORWARDED_QUERY_KEYS.contains(&k.as_str()))
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    Ok(Target { scheme: "https", host, port, path: format!("/containerLogs/{namespace}/{name}/{container}"), query })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod_with_containers(names: &[&str]) -> Value {
        json!({
            "metadata": {"name": "p1", "namespace": "default"},
            "spec": {"nodeName": "node1", "containers": names.iter().map(|n| json!({"name": n})).collect::<Vec<_>>()},
        })
    }

    fn node_with_addresses(addrs: &[(&str, &str)]) -> Value {
        json!({"status": {"addresses": addrs.iter().map(|(t, a)| json!({"type": t, "address": a})).collect::<Vec<_>>()}})
    }

    #[test]
    fn defaults_to_the_only_container() {
        let pod = pod_with_containers(&["only"]);
        assert_eq!(validate_container("", &pod), Ok("only".to_string()));
    }

    #[test]
    fn ambiguous_without_an_explicit_container() {
        let pod = pod_with_containers(&["a", "b"]);
        let err = validate_container("", &pod).unwrap_err();
        assert_eq!(err, Error::NoDefaultContainer { pod_name: "p1".to_string(), candidates: vec!["a".to_string(), "b".to_string()] });
    }

    #[test]
    fn no_containers_at_all_is_also_no_default_container() {
        let pod = pod_with_containers(&[]);
        assert!(matches!(validate_container("", &pod), Err(Error::NoDefaultContainer { .. })));
    }

    #[test]
    fn explicit_container_must_exist() {
        let pod = pod_with_containers(&["a", "b"]);
        assert_eq!(validate_container("b", &pod), Ok("b".to_string()));
        assert!(matches!(validate_container("c", &pod), Err(Error::UnknownContainer { .. })));
    }

    #[test]
    fn init_containers_count_toward_the_single_container_default() {
        let pod = json!({
            "metadata": {"name": "p1"},
            "spec": {"initContainers": [{"name": "init1"}]},
        });
        assert_eq!(validate_container("", &pod), Ok("init1".to_string()));
    }

    #[test]
    fn preferred_address_picks_hostname_over_internal_ip() {
        let node = node_with_addresses(&[("InternalIP", "10.0.0.5"), ("Hostname", "node1.local")]);
        assert_eq!(preferred_node_address(&node), Some("node1.local".to_string()));
    }

    #[test]
    fn preferred_address_falls_back_through_the_order() {
        let node = node_with_addresses(&[("ExternalIP", "1.2.3.4"), ("InternalDNS", "node1.internal")]);
        assert_eq!(preferred_node_address(&node), Some("node1.internal".to_string()));
    }

    #[test]
    fn no_address_at_all_is_a_real_error() {
        let node = json!({"status": {"addresses": []}});
        assert_eq!(preferred_node_address(&node), None);
    }

    #[test]
    fn kubelet_port_prefers_daemon_endpoints_over_default() {
        let node = json!({"status": {"daemonEndpoints": {"kubeletEndpoint": {"port": 12345}}}});
        assert_eq!(kubelet_port(&node, DEFAULT_KUBELET_PORT), 12345);
    }

    #[test]
    fn kubelet_port_falls_back_when_zero_or_absent() {
        let zero = json!({"status": {"daemonEndpoints": {"kubeletEndpoint": {"port": 0}}}});
        assert_eq!(kubelet_port(&zero, DEFAULT_KUBELET_PORT), DEFAULT_KUBELET_PORT);
        assert_eq!(kubelet_port(&json!({}), DEFAULT_KUBELET_PORT), DEFAULT_KUBELET_PORT);
    }

    #[test]
    fn log_location_builds_the_full_target() {
        let pod = pod_with_containers(&["only"]);
        let node = node_with_addresses(&[("InternalIP", "10.0.0.5")]);
        let query = vec![("follow".to_string(), "true".to_string()), ("bogus".to_string(), "x".to_string())];
        let target = log_location(&pod, &node, "", &query).unwrap();
        assert_eq!(target.scheme, "https");
        assert_eq!(target.host, "10.0.0.5");
        assert_eq!(target.port, DEFAULT_KUBELET_PORT);
        assert_eq!(target.path, "/containerLogs/default/p1/only");
        assert_eq!(target.query, "follow=true");
    }

    #[test]
    fn log_location_rejects_an_unscheduled_pod() {
        let pod = json!({"metadata": {"name": "p1"}, "spec": {"containers": [{"name": "only"}]}});
        let node = json!({});
        assert_eq!(log_location(&pod, &node, "", &[]), Err(Error::PodNotScheduled));
    }

    #[test]
    fn log_location_propagates_no_node_address() {
        let pod = pod_with_containers(&["only"]);
        let node = json!({"status": {"addresses": []}});
        assert_eq!(log_location(&pod, &node, "", &[]), Err(Error::NoNodeAddress));
    }
}
