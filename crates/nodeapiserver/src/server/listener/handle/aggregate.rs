macro_rules! handle_aggregate {
    (
        $req:ident, $storage:ident, $cache_registry:ident,
        $pure_admission:ident, $pod_node_selector_config:ident,
        $identity:ident, $service_account_authenticator:ident,
        $enforce_rbac:ident, $authorization_webhook_allowed:ident,
        $aggregation_proxy_identity:ident, $kubelet_tls:ident,
        $method:ident, $path_str:ident, $query:ident, $info:ident,
        $request_field_manager:ident, $admission_metadata:ident,
        $is_get:ident, $is_list:ident, $is_create:ident,
        $is_delete:ident, $is_update:ident, $is_watch:ident,
        $is_certificate_status_subresource:ident,
        $wants_partial_metadata:ident, $has_body:ident
    ) => {{
    // Group L: aggregated APIs (`APIService`) — a genuine live reverse
    // proxy to a real aggregated backend, with discovery merge already
    // wired through the request-time discovery path.
    // Checked before the generic verb dispatch (every other special-cased
    // route in this function is too), and before `pods/log` right below
    // since an aggregated group could in principle define its own `pods`
    // resource — the check itself costs nothing extra for the vastly more
    // common non-aggregated request (`aggregator::route::resolve` is a
    // bounded `LIST` of `APIService`s only, not per-item I/O, and a
    // request with an empty `api_group` — the core group — short-circuits
    // inside it immediately). See `aggregator::route`/`::client_tls`/
    // `::availability`/`::proxy_target`'s own doc comments for the full
    // design; `aggregate_proxy` below is the dispatch glue, same split
    // `pods/log`'s own branch already established. Discovery-shaped
    // requests are handled above, including live `/apis/{group}/{version}`
    // enumeration; only resource-shaped requests under an already-known
    // `(group, version)` reach this branch, matching real upstream's own
    // "resource requests only" scope for its aggregation proxy handler.
    if $info.is_resource_request && !$info.api_group.is_empty() {
        if let Some(mut client) = $storage.clone() {
            match aggregator::route::resolve(&mut client, &$info.api_group, &$info.api_version, Some(&$cache_registry)).await {
                Ok(Some(api_service)) => return Ok(aggregate_proxy($req, &$method, &api_service, client, &$path_str, &$query, $identity.as_ref(), $aggregation_proxy_identity.as_deref()).await),
                Ok(None) => {}
                Err(e) => warn!(path = %$path_str, error = ?e, "aggregation: looking up a matching APIService failed"),
            }
        }
    }
    // Group N: pod connection subresources are HTTP upgrades. Resolve the
    // pod and its node here, then let the streaming proxy carry the upgrade
    // through to nodelet. This must run before the generic REST branches:
    // `POST .../pods/{name}/exec` is otherwise indistinguishable from an
    // ordinary create-shaped request to the path parser.
    if $info.is_resource_request
        && $info.api_group.is_empty()
        && $info.resource == "pods"
        && !$info.name.is_empty()
        && matches!($info.subresource.as_str(), "exec" | "attach" | "portforward")
        && matches!($method.as_str(), "GET" | "POST")
    {
        let Some(mut client) = $storage.clone() else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        };
        let namespace = if $info.namespace.is_empty() { None } else { Some($info.namespace.as_str()) };

        let pod = match rest::get(&mut client, None, "", "v1", "pods", namespace, &$info.name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
            }
            Err(error) => {
                warn!(path = %$path_str, error = ?error, "proxy: fetching the pod for a streaming subresource failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
        };
        let node_name = pod.pointer("/spec/nodeName").and_then(serde_json::Value::as_str).unwrap_or("");
        if node_name.is_empty() {
            return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "pod has not yet been scheduled to a node")));
        }
        let node = match rest::get(&mut client, None, "", "v1", "nodes", None, node_name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
            Err(error) => {
                warn!(path = %$path_str, node = %node_name, error = ?error, "proxy: fetching the pod node for a streaming subresource failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
        };

        let pairs = path::parse_query(&$query);
        let target = match proxy::pod_stream::target(&pod, &node, &$info.subresource, &pairs) {
            Ok(target) => target,
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::NoDefaultContainer { pod_name, candidates })) => {
                let detail = if candidates.is_empty() {
                    format!("a container name must be specified for pod {pod_name}")
                } else {
                    format!("a container name must be specified for pod {pod_name}, choose one of: {candidates:?}")
                };
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &detail)));
            }
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::UnknownContainer { pod_name, container })) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &format!("container {container} is not valid for pod {pod_name}"))));
            }
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::PodNotScheduled)) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "pod has not yet been scheduled to a node")));
            }
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::NoNodeAddress)) => {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
            Err(proxy::pod_stream::Error::MissingPort) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "at least one port is required for port-forward")));
            }
            Err(proxy::pod_stream::Error::InvalidPort(port)) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &format!("invalid port {port}"))));
            }
        };

        return match proxy::http_client::upgrade($req, &target, $kubelet_tls).await {
            Ok(response) => Ok(response),
            Err(error) => {
                warn!(path = %$path_str, node = %node_name, error = ?error, "proxy: streaming upgrade to nodelet failed");
                Ok(json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(&$path_str, &error.to_string())))
            }
        };
    }
    // Group N: `pods/log` — a genuine live proxy to nodelet's own
    // `/containerLogs` endpoint (`crates/nodelet/src/server/logs.rs`),
    // not a stub. See `proxy::pod_log`/`proxy::client_tls`/
    // `proxy::http_client`'s own doc comments for the full design; this
    // branch is just the dispatch glue: fetch the pod, fetch its node,
    // resolve the target (`proxy::pod_log::log_location`), dial nodelet
    // for real, relay its response — status, headers, streaming body —
    // back unmodified. Checked before the generic `$is_get` handling below
    // (which requires an empty `subresource`), same "specific virtual/
    // special-cased routes before the generic verb block" ordering every
    // other early-return branch above already uses.
    if $info.is_resource_request && $info.api_group.is_empty() && $info.resource == "pods" && $info.subresource == "log" && !$info.name.is_empty() && ($method == "GET" || $method == "HEAD") {
        let Some(mut client) = $storage.clone() else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        };
        let namespace = if $info.namespace.is_empty() { None } else { Some($info.namespace.as_str()) };

        let pod = match rest::get(&mut client, None, "", "v1", "pods", namespace, &$info.name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
            }
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "proxy: fetching the pod for pods/log failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
        };
        let node_name = pod.get("spec").and_then(|s| s.get("nodeName")).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
        if node_name.is_empty() {
            return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "pod has not yet been scheduled to a node")));
        }
        // Nodes are cluster-scoped -- `namespace: None`, matching every
        // other cluster-scoped `rest::get` call in this module.
        let node = match rest::get(&mut client, None, "", "v1", "nodes", None, &node_name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                warn!(path = %$path_str, node = %node_name, "proxy: pod's own node not found for pods/log");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "proxy: fetching the pod's node for pods/log failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
        };

        let query_pairs = path::parse_query(&$query);
        let container = query_pairs.iter().find(|(k, _)| k == "container").map(|(_, v)| v.clone()).unwrap_or_default();
        let target = match proxy::pod_log::log_location(&pod, &node, &container, &query_pairs) {
            Ok(t) => t,
            Err(proxy::pod_log::Error::NoDefaultContainer { pod_name, candidates }) => {
                let detail = if candidates.is_empty() {
                    format!("a container name must be specified for pod {pod_name}")
                } else {
                    format!("a container name must be specified for pod {pod_name}, choose one of: {candidates:?}")
                };
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &detail)));
            }
            Err(proxy::pod_log::Error::UnknownContainer { pod_name, container }) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &format!("container {container} is not valid for pod {pod_name}"))));
            }
            Err(proxy::pod_log::Error::PodNotScheduled) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "pod has not yet been scheduled to a node")));
            }
            Err(proxy::pod_log::Error::NoNodeAddress) => {
                warn!(path = %$path_str, node = %node_name, "proxy: node has no address of any preferred type for pods/log");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
        };

        return match proxy::http_client::fetch(&target, $kubelet_tls).await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                warn!(path = %$path_str, node = %node_name, error = ?e, "proxy: dialing nodelet for pods/log failed");
                Ok(json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(&$path_str, &e.to_string())))
            }
        };
    }
    }};
}
