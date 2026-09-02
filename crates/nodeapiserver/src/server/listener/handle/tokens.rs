macro_rules! handle_tokens {
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
    // Group H: TokenReview is the webhook endpoint nodelet uses when a pod
    // presents its projected ServiceAccount token. It is virtual, just like
    // the authorization review resources above, and must never be written to
    // nodestore.
    if $info.is_resource_request
        && $info.api_group == "authentication.k8s.io"
        && $info.resource == "tokenreviews"
        && $info.verb == "create"
        && $info.subresource.is_empty()
    {
        if $storage.is_none() {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        }
        let body_bytes = match read_body_bytes($req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "reading the TokenReview body failed");
                return Ok(body_read_error_response(&$path_str, &e));
            }
        };
        let mut response_body: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string()))),
        };
        let token = response_body.pointer("/spec/token").and_then(serde_json::Value::as_str).unwrap_or("");
        let authenticated = $service_account_authenticator
            .as_deref()
            .and_then(|authenticator| (!token.is_empty()).then(|| authenticator.authenticate(token)).flatten());
        response_body["apiVersion"] = serde_json::json!("authentication.k8s.io/v1");
        response_body["kind"] = serde_json::json!("TokenReview");
        response_body["status"] = match authenticated {
            Some(authenticated) => serde_json::json!({
                "authenticated": true,
                "user": {
                    "username": authenticated.$identity.name,
                    "uid": authenticated.service_account_uid,
                    "groups": authenticated.$identity.groups,
                    "extra": authenticated.$identity.extra,
                }
            }),
            None => serde_json::json!({"authenticated": false}),
        };
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // Group H: ServiceAccount TokenRequest backs projected pod tokens. The
    // caller must be authorized for the serviceaccounts/token subresource;
    // the ServiceAccount and, when supplied, bound Pod are read from $storage
    // before the stateless signer is allowed to mint a token.
    if $info.is_resource_request
        && $info.api_group.is_empty()
        && $info.resource == "serviceaccounts"
        && $info.subresource == "token"
        && $info.verb == "create"
        && !$info.namespace.is_empty()
        && !$info.name.is_empty()
    {
        let Some(mut client) = $storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        };
        let Some(authenticator) = $service_account_authenticator.as_deref() else {
            return Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(&$path_str, "ServiceAccount token signing is not configured")));
        };
        let body_bytes = match read_body_bytes($req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "reading the TokenRequest body failed");
                return Ok(body_read_error_response(&$path_str, &e));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string()))),
        };
        let mut request = match crate::authn::service_account::parse_token_request(&body_value) {
            Ok(request) => request,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e))),
        };
        let service_account = match rest::get(&mut client, None, "", "v1", "serviceaccounts", Some(&$info.namespace), &$info.name).await {
            Ok(rest::GetOutcome::Found(service_account)) => service_account,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
            }
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "TokenRequest ServiceAccount lookup failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
        };
        let service_account_uid = service_account
            .pointer("/metadata/uid")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if let Some((pod_name, pod_uid)) = &request.bound_pod {
            match rest::get(&mut client, None, "", "v1", "pods", Some(&$info.namespace), pod_name).await {
                Ok(rest::GetOutcome::Found(pod)) if pod.pointer("/metadata/uid").and_then(serde_json::Value::as_str) == Some(pod_uid) => {
                    if let Some(node_name) = pod
                        .pointer("/spec/nodeName")
                        .and_then(serde_json::Value::as_str)
                        .filter(|name| !name.is_empty())
                    {
                        let node_uid = match rest::get(&mut client, None, "", "v1", "nodes", None, node_name).await {
                            Ok(rest::GetOutcome::Found(node)) => node
                                .pointer("/metadata/uid")
                                .and_then(serde_json::Value::as_str)
                                .filter(|uid| !uid.is_empty())
                                .map(str::to_string),
                            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                            Err(error) => {
                                warn!(error = ?error, node = %node_name, "TokenRequest node lookup failed; issuing the node-name claim without a node UID");
                                None
                            }
                        };
                        request.bound_pod_node = Some((node_name.to_string(), node_uid));
                    }
                }
                Ok(rest::GetOutcome::Found(_)) => {
                    return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "bound Pod UID does not match the current Pod")));
                }
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                    return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
                }
                Err(e) => {
                    warn!(path = %$path_str, error = ?e, "TokenRequest bound Pod lookup failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            }
        }
        let issued = match authenticator.issue_token(&$info.namespace, &$info.name, service_account_uid, &request) {
            Ok(issued) => issued,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string()))),
        };
        let mut response_body = body_value;
        response_body["apiVersion"] = serde_json::json!("authentication.k8s.io/v1");
        response_body["kind"] = serde_json::json!("TokenRequest");
        response_body["status"] = serde_json::json!({
            "token": issued.token,
            "expirationTimestamp": issued.expiration_timestamp,
        });
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    }};
}
