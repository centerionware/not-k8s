    match proxy::http_client::relay(&target, client_config, method, &headers, body).await {
        Ok(response) => response,
        Err(error) => {
            warn!(path = %path_str, host = %target.host, error = ?error, "proxy: dialing the backend failed");
            json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, &error.to_string()))
        }
    }
