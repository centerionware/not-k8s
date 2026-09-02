
/// Request headers this build never forwards to an aggregated backend —
/// hop-by-hop headers (`Connection`'s own listed value plus the fixed
/// standard set, RFC 7230 §6.1) and `Host` (rebuilt from the resolved
/// target instead, same as `proxy::http_client::fetch`'s own posture for
/// nodelet).
const HOP_BY_HOP_HEADERS: &[&str] = &["host", "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailers", "transfer-encoding", "upgrade"];

/// Group L Phase 4's dispatch glue for one already-matched, non-local
/// `APIService`: fetch its backing Service and `EndpointSlice`s, run the
/// same real pre-flight chain `aggregator::availability::preflight_check`
/// would run before a live discovery-endpoint dial, resolve the actual
/// dial target (`aggregator::proxy_target`), build this backend's own
/// TLS trust (`aggregator::client_tls`), and relay the whole request —
/// method, headers minus [`HOP_BY_HOP_HEADERS`], body — unmodified
/// (`proxy::http_client::relay`). A real transparent proxy, matching
/// real upstream's own aggregation posture exactly: nothing about the
/// request or response is inspected or altered beyond what dialing
/// itself requires.
///
/// A cached `Available: False` condition (`aggregator::reconcile`'s own
/// periodic write, `availability::cached_available`) short-circuits
/// straight to `503` before any of the Service/`EndpointSlice` I/O below
/// — a known-broken backend fails fast without paying for a fetch this
/// build already knows the answer to. `Available: True` or no cached
/// condition yet both fall through to the full check unchanged (the
/// backing Service still has to be fetched either way, to resolve the
/// actual dial target — this only ever saves the *negative* path).
async fn aggregate_proxy(req: Request<Incoming>, method: &str, api_service: &serde_json::Value, mut client: StorageClient, path_str: &str, query: &str) -> Response<BoxedBody> {
    if aggregator::availability::cached_available(api_service) == Some(false) {
        return json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(path_str, "backing service is not currently available (cached)"));
    }
    let Some(service_ref) = api_service.pointer("/spec/service") else {
        // `aggregator::route::resolve` already filters this out -- reached
        // only if the stored object changed between that check and here.
        return json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, "APIService has no backing service"));
    };
    let namespace = service_ref.get("namespace").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    let name = service_ref.get("name").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    let port = service_ref.get("port").and_then(serde_json::Value::as_i64).unwrap_or(443);

    let service = match rest::get(&mut client, None, "", "v1", "services", Some(&namespace), &name).await {
        Ok(rest::GetOutcome::Found(object)) => Some(object),
        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
        Err(e) => {
            warn!(path = %path_str, error = ?e, "aggregation: fetching the backing service failed");
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
        }
    };
    let endpoint_slices = match rest::list(&mut client, None, "discovery.k8s.io", "v1", "endpointslices", Some(&namespace), &format!("kubernetes.io/service-name={name}"), "", 0, "").await {
        Ok(rest::ListOutcome::Found(list)) => list.get("items").and_then(serde_json::Value::as_array).cloned().unwrap_or_default(),
        Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => Vec::new(),
        Err(e) => {
            warn!(path = %path_str, error = ?e, "aggregation: listing endpointslices for the backing service failed");
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
        }
    };
    if let Err(condition) = aggregator::availability::preflight_check(&namespace, &name, port, service.as_ref(), &endpoint_slices) {
        warn!(path = %path_str, reason = condition.reason, message = %condition.message, "aggregation: pre-flight check failed, not attempting the backend dial");
        return json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(path_str, &condition.message));
    }
    let Some(service) = service else {
        return json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(path_str, "backing service not found"));
    };

    let target = match aggregator::proxy_target::resolve(api_service, &service, path_str, query) {
        Ok(t) => t,
        Err(aggregator::proxy_target::Error::Local) => return json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, "APIService has no backing service")),
        Err(aggregator::proxy_target::Error::NoClusterIp) => return json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, "backing service has no clusterIP to dial")),
    };

    let insecure_skip_tls_verify = api_service.pointer("/spec/insecureSkipTLSVerify").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let ca_bundle_pem = match api_service.pointer("/spec/caBundle").and_then(serde_json::Value::as_str) {
        Some(b64) if !b64.is_empty() => {
            use base64::Engine;
            match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "aggregation: spec.caBundle is not valid base64");
                    return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
                }
            }
        }
        _ => None,
    };
    let client_config = match aggregator::client_tls::build_client_config(ca_bundle_pem.as_deref(), insecure_skip_tls_verify) {
        Ok(cfg) => std::sync::Arc::new(cfg),
        Err(e) => {
            warn!(path = %path_str, error = ?e, "aggregation: building the backend TLS client config failed");
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
        }
    };

    let headers = req
        .headers()
        .iter()
        .filter(|(name, _)| !HOP_BY_HOP_HEADERS.contains(&name.as_str().to_ascii_lowercase().as_str()))
        .filter_map(|(name, value)| value.to_str().ok().map(|v| (name.as_str().to_string(), v.to_string())))
        .collect::<Vec<_>>();
    let body = match read_body_bytes(req).await {
        Ok(b) => b,
        Err(e) => {
            warn!(path = %path_str, error = ?e, "aggregation: reading the request body failed");
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
        }
    };

    match proxy::http_client::relay(&target, client_config, method, &headers, body).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(path = %path_str, host = %target.host, error = ?e, "aggregation: dialing the backend failed");
            json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, &e.to_string()))
        }
    }
}

fn is_proxy_request(info: &path::RequestInfo) -> bool {
    info.verb == "proxy" || info.subresource == "proxy"
}

/// Returns the path after the `proxy` marker.  `RequestInfo.parts` has
/// already removed the API prefix, group/version, and optional namespace,
/// so this handles both supported Kubernetes forms:
/// `.../{resource}/{name}/proxy/{path}` and
/// `.../proxy/{resource}/{name}/{path}`.
fn proxy_suffix(info: &path::RequestInfo) -> String {
    let start = if info.verb == "proxy" {
        2
    } else {
        info.parts.iter().position(|part| part == "proxy").map_or(info.parts.len(), |index| index + 1)
    };
    let suffix = info.parts.get(start..).map(|parts| parts.join("/")).unwrap_or_default();
    if suffix.is_empty() { "/".to_string() } else { format!("/{suffix}") }
}

/// Group N's core node/service proxy dispatch.  The object and EndpointSlice
/// reads are intentionally performed before consuming the request body so an
/// invalid or unavailable target returns a normal Kubernetes Status response.
async fn proxy_resource(
    req: Request<Incoming>,
    storage: Option<StorageClient>,
    info: &path::RequestInfo,
    method: &str,
    path_str: &str,
    query: &str,
    identity: &Option<crate::authn::x509::Identity>,
    enforce_rbac: bool,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Response<BoxedBody> {
    let Some(mut client) = storage else {
        return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
    };

    if enforce_rbac {
        let (user_name, user_groups): (&str, Vec<String>) = match identity {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, &info.namespace).await;
        let subresource = if info.verb == "proxy" { "proxy" } else { info.subresource.as_str() };
        let attrs = authz::rbac::RequestAttributes {
            is_resource_request: true,
            verb: &info.verb,
            api_group: &info.api_group,
            resource: &info.resource,
            subresource,
            name: &info.name,
            path: path_str,
        };
        if !authz::rbac::rules_allow(&attrs, &resolved.rules) {
            return json_response(StatusCode::FORBIDDEN, &forbidden_status(path_str, user_name));
        }
    }

    let suffix = proxy_suffix(info);
    let target = if info.resource == "nodes" {
        let node = match rest::get(&mut client, None, "", "v1", "nodes", None, &info.name).await {
            Ok(rest::GetOutcome::Found(node)) => node,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return json_response(StatusCode::NOT_FOUND, &not_found_status(path_str));
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "proxy: fetching the node failed");
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
            }
        };
        match proxy::node_proxy::target(&node, &suffix, query) {
            Ok(target) => target,
            Err(proxy::node_proxy::Error::NoNodeAddress) => {
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
            }
        }
    } else {
        if info.namespace.is_empty() {
            return json_response(StatusCode::BAD_REQUEST, &bad_request_status(path_str, "service proxy requires a namespace"));
        }
        let (service_name, _) = proxy::service_proxy::split_name(&info.name);
        let service = match rest::get(&mut client, None, "", "v1", "services", Some(&info.namespace), service_name).await {
            Ok(rest::GetOutcome::Found(service)) => service,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return json_response(StatusCode::NOT_FOUND, &not_found_status(path_str));
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "proxy: fetching the Service failed");
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
            }
        };
        let endpoint_slices = match rest::list(&mut client, None, "discovery.k8s.io", "v1", "endpointslices", Some(&info.namespace), &format!("kubernetes.io/service-name={service_name}"), "", 0, "").await {
            Ok(rest::ListOutcome::Found(list)) => list.get("items").and_then(serde_json::Value::as_array).cloned().unwrap_or_default(),
            Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => Vec::new(),
            Err(error) => {
                warn!(path = %path_str, error = ?error, "proxy: listing EndpointSlices failed");
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
            }
        };
        match proxy::service_proxy::target(&service, &endpoint_slices, &info.name, &suffix, query) {
            Ok(target) => target,
            Err(proxy::service_proxy::Error::MissingPort
                | proxy::service_proxy::Error::InvalidPort(_)
                | proxy::service_proxy::Error::UnsupportedProtocol(_)) =>
            {
                return json_response(StatusCode::BAD_REQUEST, &bad_request_status(path_str, "the requested Service port does not exist"));
            }
            Err(proxy::service_proxy::Error::NoClusterIpOrEndpoint) => {
                return json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(path_str, "Service has no ready endpoints or ClusterIP"));
            }
        }
    };

    let headers = req
        .headers()
        .iter()
        .filter(|(name, _)| !HOP_BY_HOP_HEADERS.contains(&name.as_str()))
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str().to_string(), value.to_string())))
        .collect::<Vec<_>>();
    let body = match read_body_bytes(req).await {
        Ok(body) => body,
        Err(error) => {
            warn!(path = %path_str, error = ?error, "proxy: reading the request body failed");
            return json_response(StatusCode::BAD_REQUEST, &bad_request_status(path_str, "request body could not be read"));
        }
    };

    let client_config = if target.scheme == "https" && info.resource == "services" {
        match crate::proxy::client_tls::build_client_config(None) {
            Ok(config) => std::sync::Arc::new(config),
            Err(error) => {
                warn!(path = %path_str, error = ?error, "proxy: building the Service TLS client config failed");
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
            }
        }
    } else {
        // Node proxies use the kubelet client configuration built at
        // listener startup.  Plain HTTP Service targets ignore it.
        kubelet_tls
    };

    match proxy::http_client::relay(&target, client_config, method, &headers, body).await {
        Ok(response) => response,
        Err(error) => {
            warn!(path = %path_str, host = %target.host, error = ?error, "proxy: dialing the backend failed");
            json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, &error.to_string()))
        }
    }
}
