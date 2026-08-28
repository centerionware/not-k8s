//! Target resolution for Kubernetes' node and Service `proxy` subresources.
//!
//! The API exposes two equivalent URL families: the canonical
//! `.../{resource}/{name}/proxy/{path}` form and the older
//! `.../proxy/{resource}/{name}/{path}` form. The listener resolves the
//! referenced Node or Service before handing the target to the HTTP relay.

use crate::proxy::pod_log::{kubelet_port, preferred_node_address, Target, DEFAULT_KUBELET_PORT};
use serde_json::Value;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("the node has no preferred address")]
    NoNodeAddress,
    #[error("the Service has no ClusterIP")]
    NoClusterIp,
    #[error("the Service proxy name must include a port")]
    MissingPort,
    #[error("the Service has no port matching {0:?}")]
    UnknownPort(String),
    #[error("the proxy path is malformed")]
    InvalidPath,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProxyRoute {
    pub resource: &'static str,
    pub namespace: Option<String>,
    pub name: String,
    pub path: String,
}

/// Builds a TLS target for the node's kubelet-style HTTP server. The kubelet
/// port and address selection are the same ones used by `pods/log`.
pub fn node_target(node: &Value, path: &str, query: &str) -> Result<Target, Error> {
    let host = preferred_node_address(node).ok_or(Error::NoNodeAddress)?;
    let port = kubelet_port(node, DEFAULT_KUBELET_PORT);
    Ok(Target {
        scheme: "https",
        host,
        port,
        path: normalize_path(path),
        query: query.to_string(),
    })
}

/// Builds a plaintext target for a Service ClusterIP. Service proxying is
/// deliberately sent through the ClusterIP, matching kube-apiserver's
/// default resolver and allowing nodeproxy's normal routing to pick the
/// endpoint.
pub fn service_target(
    service: &Value,
    requested_port: &str,
    path: &str,
    query: &str,
) -> Result<Target, Error> {
    let host = service
        .pointer("/spec/clusterIP")
        .and_then(Value::as_str)
        .filter(|ip| !ip.is_empty() && *ip != "None")
        .ok_or(Error::NoClusterIp)?
        .to_string();
    let ports = service
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let port = ports
        .iter()
        .find(|port| {
            port.get("name").and_then(Value::as_str) == Some(requested_port)
                || port
                    .get("port")
                    .and_then(Value::as_i64)
                    .is_some_and(|p| p.to_string() == requested_port)
        })
        .and_then(|port| port.get("port").and_then(Value::as_i64))
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| Error::UnknownPort(requested_port.to_string()))?;
    Ok(Target {
        scheme: "http",
        host,
        port,
        path: normalize_path(path),
        query: query.to_string(),
    })
}

/// Splits the Service URL's `{name}:{port}` path component. A named Service
/// port is kept as-is and resolved against `spec.ports` later.
pub fn service_name_and_port(value: &str) -> Result<(&str, &str), Error> {
    let (name, port) = value.rsplit_once(':').ok_or(Error::MissingPort)?;
    if name.is_empty() || port.is_empty() {
        return Err(Error::MissingPort);
    }
    Ok((name, port))
}

/// Resolves either supported URL family into the referenced object and the
/// path to send to it. Keeping this independent of [`RequestInfo`] matters
/// for the legacy namespaced Service form: the general path parser quite
/// correctly treats its namespace as the object name because that URL uses
/// `proxy` as a path-prefix verb rather than a subresource.
pub fn route(parts: &[String]) -> Result<ProxyRoute, Error> {
    let (resource, namespace, name, suffix_start) = if parts.get(2).map(String::as_str)
        == Some("proxy")
    {
        match (
            parts.get(3).map(String::as_str),
            parts.get(4),
            parts.get(5).map(String::as_str),
            parts.get(6),
        ) {
            (Some("nodes"), Some(name), _, _) => ("nodes", None, name.clone(), 5),
            (Some("namespaces"), Some(namespace), Some("services"), Some(name)) => {
                ("services", Some(namespace.clone()), name.clone(), 7)
            }
            _ => return Err(Error::InvalidPath),
        }
    } else {
        match (
            parts.get(2).map(String::as_str),
            parts.get(3),
            parts.get(4).map(String::as_str),
            parts.get(5),
            parts.get(6).map(String::as_str),
        ) {
            (Some("nodes"), Some(name), Some("proxy"), _, _) => ("nodes", None, name.clone(), 5),
            (Some("namespaces"), Some(namespace), Some("services"), Some(name), Some("proxy")) => {
                ("services", Some(namespace.clone()), name.clone(), 7)
            }
            _ => return Err(Error::InvalidPath),
        }
    };
    if name.is_empty() || parts.len() < suffix_start {
        return Err(Error::InvalidPath);
    }
    let path = if parts.len() == suffix_start {
        "/".to_string()
    } else {
        format!("/{}", parts[suffix_start..].join("/"))
    };
    Ok(ProxyRoute {
        resource,
        namespace,
        name,
        path,
    })
}

/// Returns the path after the `proxy` marker in either supported URL form.
pub fn suffix_after_proxy(parts: &[String]) -> Result<String, Error> {
    Ok(route(parts)?.path)
}

fn normalize_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn service() -> Value {
        json!({"spec": {"clusterIP": "10.43.0.10", "ports": [{"name": "http", "port": 8080}, {"name": "https", "port": 8443}]}})
    }

    fn node() -> Value {
        json!({"status": {"addresses": [{"type": "InternalIP", "address": "192.0.2.10"}], "daemonEndpoints": {"kubeletEndpoint": {"port": 10255}}}})
    }

    #[test]
    fn resolves_a_named_service_port_over_plain_http() {
        let target = service_target(&service(), "http", "healthz", "x=1").unwrap();
        assert_eq!(target.scheme, "http");
        assert_eq!(target.host, "10.43.0.10");
        assert_eq!(target.port, 8080);
        assert_eq!(target.path, "/healthz");
        assert_eq!(target.query, "x=1");
    }

    #[test]
    fn resolves_a_node_using_the_kubelet_port() {
        let target = node_target(&node(), "/metrics", "").unwrap();
        assert_eq!(target.scheme, "https");
        assert_eq!(target.host, "192.0.2.10");
        assert_eq!(target.port, 10255);
    }

    #[test]
    fn rejects_a_headless_service_and_unknown_port() {
        let headless = json!({"spec": {"clusterIP": "None", "ports": [{"port": 80}]}});
        assert_eq!(
            service_target(&headless, "80", "/", ""),
            Err(Error::NoClusterIp)
        );
        assert_eq!(
            service_target(&service(), "9090", "/", ""),
            Err(Error::UnknownPort("9090".to_string()))
        );
    }

    #[test]
    fn recognizes_both_proxy_path_families() {
        let canonical = vec![
            "api".into(),
            "v1".into(),
            "nodes".into(),
            "n1".into(),
            "proxy".into(),
            "metrics".into(),
        ];
        let legacy = vec![
            "api".into(),
            "v1".into(),
            "proxy".into(),
            "nodes".into(),
            "n1".into(),
            "metrics".into(),
        ];
        assert_eq!(suffix_after_proxy(&canonical).unwrap(), "/metrics");
        assert_eq!(suffix_after_proxy(&legacy).unwrap(), "/metrics");
        assert_eq!(
            suffix_after_proxy(&["api".into(), "v1".into(), "nodes".into()]),
            Err(Error::InvalidPath)
        );
    }

    #[test]
    fn resolves_namespaced_service_in_both_proxy_path_families() {
        let canonical = vec![
            "api".into(),
            "v1".into(),
            "namespaces".into(),
            "default".into(),
            "services".into(),
            "web:http".into(),
            "proxy".into(),
            "health".into(),
        ];
        let legacy = vec![
            "api".into(),
            "v1".into(),
            "proxy".into(),
            "namespaces".into(),
            "default".into(),
            "services".into(),
            "web:http".into(),
            "health".into(),
        ];
        for parts in [canonical, legacy] {
            assert_eq!(
                route(&parts).unwrap(),
                ProxyRoute {
                    resource: "services",
                    namespace: Some("default".to_string()),
                    name: "web:http".to_string(),
                    path: "/health".to_string(),
                }
            );
        }
    }
}
