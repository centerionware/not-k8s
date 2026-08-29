//! Target resolution for the core `services/{name}/proxy` subresource.
//!
//! Kubernetes accepts an optional `:port-name` (or numeric `:port`) suffix
//! on a service proxy name.  When an EndpointSlice is available, resolving
//! to a ready endpoint makes the subresource work even when the API server
//! is deployed without a local Service proxy.  A Service ClusterIP remains
//! the fallback, matching the normal kube-apiserver service-proxy path when
//! endpoint data is temporarily absent.

use crate::proxy::pod_log::Target;
use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    MissingPort,
    InvalidPort(String),
    UnsupportedProtocol(String),
    NoClusterIpOrEndpoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePort {
    pub name: Option<String>,
    pub port: u16,
    pub protocol: String,
    pub app_protocol: Option<String>,
}

/// Splits the API's `service[:port-name]` object-name form.
pub fn split_name(name: &str) -> (&str, Option<&str>) {
    name.rsplit_once(':')
        .map_or((name, None), |(service, port)| (service, Some(port)))
}

fn select_port(service: &Value, selector: Option<&str>) -> Result<ServicePort, Error> {
    let ports = service
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .ok_or(Error::MissingPort)?;
    let selected = match selector {
        None => ports.first(),
        Some(value) => {
            let numeric = value.parse::<u16>().ok();
            ports.iter().find(|port| {
                numeric.is_some_and(|number| {
                    port.get("port").and_then(Value::as_u64) == Some(number as u64)
                }) || port.get("name").and_then(Value::as_str) == Some(value)
            })
        }
    }
    .ok_or_else(|| {
        selector.map_or(Error::MissingPort, |value| {
            Error::InvalidPort(value.to_string())
        })
    })?;

    let port = selected
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .ok_or(Error::MissingPort)?;
    Ok(ServicePort {
        name: selected
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        port,
        protocol: selected
            .get("protocol")
            .and_then(Value::as_str)
            .unwrap_or("TCP")
            .to_string(),
        app_protocol: selected
            .get("appProtocol")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn endpoint_ready(endpoint: &Value) -> bool {
    let conditions = endpoint.get("conditions");
    let terminating = conditions
        .and_then(|c| c.get("terminating"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ready = conditions
        .and_then(|c| c.get("ready"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let serving = conditions
        .and_then(|c| c.get("serving"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    !terminating && (ready || serving)
}

fn endpoint_for_port(
    endpoint_slices: &[Value],
    service_port: &ServicePort,
) -> Option<(String, u16)> {
    endpoint_slices.iter().find_map(|slice| {
        let ports = slice.get("ports").and_then(Value::as_array)?;
        let endpoint_port = ports.iter().find(|port| {
            let name_matches = match service_port.name.as_deref() {
                Some(name) => port.get("name").and_then(Value::as_str) == Some(name),
                None => {
                    port.get("name").is_none()
                        || port.get("name").and_then(Value::as_str) == Some("")
                }
            };
            name_matches
                && port.get("port").and_then(Value::as_u64).is_some()
                && port
                    .get("protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("TCP")
                    == service_port.protocol
        })?;
        let port = u16::try_from(endpoint_port.get("port")?.as_u64()?).ok()?;
        let endpoint = slice
            .get("endpoints")
            .and_then(Value::as_array)?
            .iter()
            .find(|endpoint| endpoint_ready(endpoint))?;
        let address = endpoint
            .get("addresses")
            .and_then(Value::as_array)?
            .iter()
            .find_map(Value::as_str)?;
        Some((address.to_string(), port))
    })
}

fn scheme(service_port: &ServicePort) -> &'static str {
    match service_port.app_protocol.as_deref() {
        Some("https") => "https",
        _ => "http",
    }
}

/// Resolves one service proxy request to a ready EndpointSlice address, or
/// to the Service ClusterIP when no ready endpoint is currently available.
pub fn target(
    service: &Value,
    endpoint_slices: &[Value],
    name: &str,
    path: &str,
    query: &str,
) -> Result<Target, Error> {
    let (service_name, selector) = split_name(name);
    if service_name.is_empty() {
        return Err(Error::NoClusterIpOrEndpoint);
    }
    let service_port = select_port(service, selector)?;
    if service_port.protocol != "TCP" {
        return Err(Error::UnsupportedProtocol(service_port.protocol));
    }
    let path = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

    if let Some((host, port)) = endpoint_for_port(endpoint_slices, &service_port) {
        return Ok(Target {
            scheme: scheme(&service_port),
            host,
            port,
            path,
            query: query.to_string(),
        });
    }

    let cluster_ip = service
        .pointer("/spec/clusterIP")
        .and_then(Value::as_str)
        .filter(|ip| !ip.is_empty() && *ip != "None")
        .ok_or(Error::NoClusterIpOrEndpoint)?;
    Ok(Target {
        scheme: scheme(&service_port),
        host: cluster_ip.to_string(),
        port: service_port.port,
        path,
        query: query.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn service() -> Value {
        json!({"spec": {"clusterIP": "10.43.0.20", "ports": [
            {"name": "http", "port": 80, "targetPort": 8080},
            {"name": "https", "port": 443, "appProtocol": "https"}
        ]}})
    }

    fn endpoints() -> Vec<Value> {
        vec![
            json!({"ports": [{"name": "http", "port": 8080}], "endpoints": [{"addresses": ["10.42.0.7"], "conditions": {"ready": true}}]}),
        ]
    }

    #[test]
    fn splits_optional_service_port() {
        assert_eq!(split_name("web:http"), ("web", Some("http")));
        assert_eq!(split_name("web:8080"), ("web", Some("8080")));
        assert_eq!(split_name("web"), ("web", None));
    }

    #[test]
    fn resolves_a_ready_endpoint_and_preserves_the_suffix() {
        let target = target(&service(), &endpoints(), "web:http", "/healthz", "x=1").unwrap();
        assert_eq!(target.scheme, "http");
        assert_eq!(target.host, "10.42.0.7");
        assert_eq!(target.port, 8080);
        assert_eq!(target.path, "/healthz");
        assert_eq!(target.query, "x=1");
    }

    #[test]
    fn falls_back_to_the_cluster_ip_when_endpoints_are_not_ready() {
        let target = target(&service(), &[json!({"ports": [{"name": "http", "port": 8080}], "endpoints": [{"addresses": ["10.42.0.7"], "conditions": {"ready": false}}]})], "web:http", "", "").unwrap();
        assert_eq!(target.host, "10.43.0.20");
        assert_eq!(target.port, 80);
    }

    #[test]
    fn app_protocol_selects_https_for_the_backend() {
        let target = target(&service(), &[], "web:https", "/", "").unwrap();
        assert_eq!(target.scheme, "https");
        assert_eq!(target.port, 443);
    }

    #[test]
    fn headless_service_requires_a_ready_endpoint() {
        let mut service = service();
        service["spec"]["clusterIP"] = json!("None");
        assert_eq!(
            target(&service, &[], "web:http", "/", ""),
            Err(Error::NoClusterIpOrEndpoint)
        );
    }
}
