//! Target resolution for the pod connection subresources: `exec`, `attach`,
//! and `portforward`.
//!
//! The API server does not run a container runtime itself. It resolves the
//! pod's node exactly as the `pods/log` proxy does, translates the API query
//! into nodelet's kubelet-style route, and [`super::http_client::upgrade`]
//! carries the client's HTTP upgrade through to nodelet.

use super::pod_log::{self, Error as PodError, Target, DEFAULT_KUBELET_PORT};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;

const EXEC_QUERY_KEYS: &[&str] = &["command", "stdin", "stdout", "stderr", "input", "output", "error", "tty"];
const ATTACH_QUERY_KEYS: &[&str] = &["stdin", "stdout", "stderr", "input", "output", "error", "tty"];

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Pod(PodError),
    MissingPort,
    InvalidPort(String),
}

fn encoded_query(pairs: impl IntoIterator<Item = (String, String)>) -> String {
    pairs
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                utf8_percent_encode(&key, NON_ALPHANUMERIC),
                utf8_percent_encode(&value, NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn container_query(pairs: &[(String, String)], pod: &Value) -> Result<String, Error> {
    let name = pairs
        .iter()
        .find(|(key, _)| key == "container")
        .map(|(_, value)| value.as_str())
        .unwrap_or("");
    pod_log::validate_container(name, pod).map_err(Error::Pod)
}

fn node_target(pod: &Value, node: &Value) -> Result<(String, u16, String, String), Error> {
    if pod.pointer("/spec/nodeName").and_then(Value::as_str).filter(|name| !name.is_empty()).is_none() {
        return Err(Error::Pod(PodError::PodNotScheduled));
    }
    let host = pod_log::preferred_node_address(node).ok_or(Error::Pod(PodError::NoNodeAddress))?;
    let port = pod_log::kubelet_port(node, DEFAULT_KUBELET_PORT);
    let namespace = pod
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = pod
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((host, port, namespace, name))
}

/// Build the nodelet target for one of Kubernetes' upgraded pod
/// subresources. The API uses `stdin`/`stdout`/`stderr`, while nodelet's
/// kubelet-style route accepts the equivalent `input`/`output`/`error` names;
/// translating here keeps both endpoints compatible with their native
/// clients.
pub fn target(pod: &Value, node: &Value, subresource: &str, pairs: &[(String, String)]) -> Result<Target, Error> {
    let (host, port, namespace, name) = node_target(pod, node)?;
    let (path, query) = match subresource {
        "exec" | "attach" => {
            let container = container_query(pairs, pod)?;
            let keys = if subresource == "exec" { EXEC_QUERY_KEYS } else { ATTACH_QUERY_KEYS };
            let translated = pairs
                .iter()
                .filter(|(key, _)| keys.contains(&key.as_str()))
                .map(|(key, value)| {
                    let key = match key.as_str() {
                        "stdin" => "input",
                        "stdout" => "output",
                        "stderr" => "error",
                        other => other,
                    };
                    (key.to_string(), value.clone())
                });
            (format!("/{subresource}/{namespace}/{name}/{container}"), encoded_query(translated))
        }
        "portforward" => {
            let mut ports = Vec::new();
            for (_, value) in pairs.iter().filter(|(key, _)| key == "ports" || key == "port") {
                for port in value.split(',').filter(|port| !port.is_empty()) {
                    let parsed = port.parse::<u16>().map_err(|_| Error::InvalidPort(port.to_string()))?;
                    ports.push(parsed.to_string());
                }
            }
            if ports.is_empty() {
                return Err(Error::MissingPort);
            }
            let query = ports.into_iter().map(|port| ("port".to_string(), port));
            (format!("/portForward/{namespace}/{name}"), encoded_query(query))
        }
        other => return Err(Error::InvalidPort(format!("unsupported pod subresource {other}"))),
    };
    Ok(Target { scheme: "https", host, port, path, query })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod() -> Value {
        json!({
            "metadata": {"namespace": "default", "name": "demo"},
            "spec": {"nodeName": "node-a", "containers": [{"name": "app"}]}
        })
    }

    fn node() -> Value {
        json!({"status": {"addresses": [{"type": "InternalIP", "address": "10.0.0.5"}]}})
    }

    #[test]
    fn exec_target_translates_api_stream_flags() {
        let pairs = vec![
            ("container".to_string(), "app".to_string()),
            ("stdout".to_string(), "true".to_string()),
            ("command".to_string(), "echo".to_string()),
            ("command".to_string(), "hello world".to_string()),
        ];
        let target = target(&pod(), &node(), "exec", &pairs).unwrap();
        assert_eq!(target.host, "10.0.0.5");
        assert_eq!(target.path, "/exec/default/demo/app");
        assert_eq!(target.query, "output=true&command=echo&command=hello%20world");
    }

    #[test]
    fn portforward_target_normalizes_the_plural_ports_parameter() {
        let pairs = vec![("ports".to_string(), "80,1234".to_string())];
        let target = target(&pod(), &node(), "portforward", &pairs).unwrap();
        assert_eq!(target.path, "/portForward/default/demo");
        assert_eq!(target.query, "port=80&port=1234");
    }

    #[test]
    fn portforward_requires_a_port() {
        assert_eq!(target(&pod(), &node(), "portforward", &[]), Err(Error::MissingPort));
    }
}
