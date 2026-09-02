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

