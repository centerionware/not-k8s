//! Path routing + top-level request dispatch for the kubelet-style server.
//! Matches real kubelet's own path shapes exactly (`/containerLogs/{ns}/
//! {pod}/{container}`, `/exec/{ns}/{pod}/{container}`, etc.) since that's
//! what the apiserver's kubelet proxy sends unmodified.

use super::{auth, exec, logs, prom_metrics, stats, text_response, BoxedBody, ServerState};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    ContainerLogs { namespace: String, pod: String, container: String },
    Exec { namespace: String, pod: String, container: String },
    Attach { namespace: String, pod: String, container: String },
    PortForward { namespace: String, pod: String },
    StatsSummary,
    MetricsResource,
    MetricsCadvisor,
    NotFound,
}

/// Parse a request path into a `Route`. Pure — no query-string handling
/// here (see `parse_query` below), just the path-segment shape.
pub fn parse_route(path: &str) -> Route {
    let segments: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["containerLogs", ns, pod, container] => {
            Route::ContainerLogs { namespace: ns.to_string(), pod: pod.to_string(), container: container.to_string() }
        }
        ["exec", ns, pod, container] => {
            Route::Exec { namespace: ns.to_string(), pod: pod.to_string(), container: container.to_string() }
        }
        ["attach", ns, pod, container] => {
            Route::Attach { namespace: ns.to_string(), pod: pod.to_string(), container: container.to_string() }
        }
        ["portForward", ns, pod] => Route::PortForward { namespace: ns.to_string(), pod: pod.to_string() },
        ["stats", "summary"] => Route::StatsSummary,
        ["metrics", "resource"] => Route::MetricsResource,
        ["metrics", "cadvisor"] => Route::MetricsCadvisor,
        _ => Route::NotFound,
    }
}

fn percent_decode(s: &str) -> String {
    // `+` means space in a query string (application/x-www-form-urlencoded
    // convention) — percent_encoding's decoder doesn't do that on its own,
    // it only handles %XX escapes.
    let replaced = s.replace('+', " ");
    percent_encoding::percent_decode_str(&replaced).decode_utf8_lossy().into_owned()
}

/// Parse a query string (`a=1&b=2`) into key/value pairs, percent-decoded.
/// Repeated keys (`command=sh&command=-c&command=...`) all survive, in
/// order — that's how kubectl encodes a multi-argument exec command.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(kv), String::new()),
        })
        .collect()
}

pub fn query_flag(pairs: &[(String, String)], key: &str) -> bool {
    pairs.iter().any(|(k, v)| k == key && (v == "1" || v == "true"))
}

pub fn query_value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

pub fn query_values<'a>(pairs: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    pairs.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str()).collect()
}

pub async fn handle(
    state: Arc<ServerState>,
    req: Request<Incoming>,
    client_cert_identity: Option<(String, Vec<String>)>,
) -> Result<Response<BoxedBody>, Infallible> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let route = parse_route(&path);

    if route == Route::NotFound {
        return Ok(text_response(StatusCode::NOT_FOUND, "not found"));
    }

    // A verified client certificate (chains to NODELET_CLIENT_CA_FILE)
    // authenticates directly — no TokenReview round-trip, matching real
    // kubelet's own x509-then-bearer-token authenticator chain.
    let username = if let Some((cert_username, _groups)) = client_cert_identity {
        cert_username
    } else {
        let token = auth::extract_bearer_token(req.headers().get("Authorization").and_then(|v| v.to_str().ok()));
        let Some(token) = token else {
            return Ok(text_response(StatusCode::UNAUTHORIZED, "missing or malformed Authorization: Bearer <token> header"));
        };
        match auth::authenticate(&state.client, token).await {
            Ok(Some(u)) => u,
            Ok(None) => return Ok(text_response(StatusCode::UNAUTHORIZED, "token did not authenticate")),
            Err(e) => {
                warn!(error = ?e, "server: TokenReview call failed");
                return Ok(text_response(StatusCode::INTERNAL_SERVER_ERROR, "authentication check failed"));
            }
        }
    };
    tracing::debug!(user = %username, path = %path, "server: authenticated request");

    let response = match route {
        Route::ContainerLogs { namespace, pod, container } => {
            logs::handle_container_logs(&state, &namespace, &pod, &container, &parse_query(&query)).await
        }
        Route::Exec { namespace, pod, container } => {
            exec::handle_exec(state, req, &namespace, &pod, &container, &parse_query(&query)).await
        }
        Route::Attach { namespace, pod, container } => {
            exec::handle_attach(state, req, &namespace, &pod, &container, &parse_query(&query)).await
        }
        Route::PortForward { namespace, pod } => {
            exec::handle_port_forward(state, req, &namespace, &pod, &parse_query(&query)).await
        }
        Route::StatsSummary => stats::handle_stats_summary(&state).await,
        Route::MetricsResource => prom_metrics::handle_metrics_resource(&state).await,
        Route::MetricsCadvisor => prom_metrics::handle_metrics_cadvisor(&state).await,
        Route::NotFound => unreachable!("handled above"),
    };
    Ok(response)
}

#[cfg(test)]
#[path = "routes_tests/parse_route.rs"]
mod tests_parse_route;
#[cfg(test)]
#[path = "routes_tests/parse_query.rs"]
mod tests_parse_query;
