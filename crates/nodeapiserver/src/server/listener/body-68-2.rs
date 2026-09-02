    match proxy::http_client::relay(&target, client_config, method, &headers, body).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(path = %path_str, host = %target.host, error = ?e, "aggregation: dialing the backend failed");
            json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, &e.to_string()))
        }
    }
