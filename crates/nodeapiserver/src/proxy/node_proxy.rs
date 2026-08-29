//! Target resolution for the core `nodes/{name}/proxy` subresource.
//!
//! A node proxy request is forwarded to the node's kubelet-style HTTPS
//! listener.  The listener already has the same connection-info data that
//! the `pods/log` proxy uses, so this module deliberately contains only the
//! pure resolution half; `http_client` owns the actual dial and relay.

use crate::proxy::pod_log::{kubelet_port, preferred_node_address, Target, DEFAULT_KUBELET_PORT};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NoNodeAddress,
}

/// Builds the kubelet target for a node proxy request.
pub fn target(node: &Value, path: &str, query: &str) -> Result<Target, Error> {
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

    #[test]
    fn node_proxy_uses_the_node_connection_info() {
        let node = json!({
            "status": {
                "addresses": [{"type": "InternalIP", "address": "10.0.0.9"}],
                "daemonEndpoints": {"kubeletEndpoint": {"port": 12345}}
            }
        });
        let target = target(&node, "/stats/summary", "verbose=true").unwrap();
        assert_eq!(target.scheme, "https");
        assert_eq!(target.host, "10.0.0.9");
        assert_eq!(target.port, 12345);
        assert_eq!(target.path, "/stats/summary");
        assert_eq!(target.query, "verbose=true");
    }

    #[test]
    fn node_proxy_normalizes_an_empty_suffix() {
        let node = json!({"status": {"addresses": [{"type": "Hostname", "address": "node-a"}]}});
        assert_eq!(target(&node, "", "").unwrap().path, "/");
        assert_eq!(
            target(&node, "stats/summary", "").unwrap().path,
            "/stats/summary"
        );
    }

    #[test]
    fn node_proxy_requires_a_node_address() {
        assert_eq!(target(&json!({}), "/", ""), Err(Error::NoNodeAddress));
    }
}
