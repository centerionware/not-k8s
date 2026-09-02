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

