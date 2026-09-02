macro_rules! handle_scale {
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
    // Built-in workload scale subresources expose a virtual
    // `autoscaling/v1 Scale`, not the parent object itself. Keep this ahead
    // of generic CRUD so HPA and `kubectl scale` can read and update
    // `spec.replicas` without persisting a second object.
    if $info.is_resource_request
        && $info.subresource == "scale"
        && !$info.name.is_empty()
        && rest::supports_scale(&$info.api_group, &$info.api_version, &$info.resource)
        && matches!($info.verb.as_str(), "get" | "update" | "patch")
    {
        let Some(mut client) = $storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        };
        let namespace = (!$info.namespace.is_empty()).then_some($info.namespace.as_str());
        if $info.verb == "get" {
            return match rest::get_scale(&mut client, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name).await {
                Ok(outcome) => Ok(scale_outcome_response(&$path_str, outcome)),
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, "rest::get_scale failed");
                    Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
                }
            };
        }

        let content_type = $req.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_string);
        let kind_of_patch = if $info.verb == "patch" {
            match content_type.as_deref() {
                Some(content_type) => match rest::patch_kind_for_content_type(content_type) {
                    Some(kind) => Some(kind),
                    None => {
                        return Ok(json_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, &bad_request_status(&$path_str, "unsupported Content-Type for the Scale subresource")));
                    }
                },
                None => Some(rest::PatchKind::StrategicMerge),
            }
        } else {
            None
        };
        let body_bytes = match read_body_bytes($req).await {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(path = %$path_str, error = ?error, "reading the Scale request failed");
                return Ok(body_read_error_response(&$path_str, &error));
            }
        };
        let body: Value = if $info.verb == "update" && content_type.as_deref().and_then(negotiation::content_type) == Some(negotiation::Format::Yaml) {
            match crate::codec::yaml::decode(&body_bytes) {
                Ok(body) => body,
                Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &error.to_string()))),
            }
        } else {
            match crate::codec::json::decode(&body_bytes) {
                Ok(body) => body,
                Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &error.to_string()))),
            }
        };
        let dry_run = match dry_run_query(&$query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, detail))),
        };
        let outcome = if $info.verb == "update" {
            rest::update_scale(
                &mut client,
                &$info.api_group,
                &$info.api_version,
                &$info.resource,
                namespace,
                &$info.name,
                &body,
                dry_run,
            )
            .await
        } else if let Some(kind_of_patch) = kind_of_patch {
            rest::patch_scale(
                &mut client,
                &$info.api_group,
                &$info.api_version,
                &$info.resource,
                namespace,
                &$info.name,
                kind_of_patch,
                &body,
                dry_run,
            )
            .await
        } else {
            unreachable!("scale PATCH requests always have a patch kind")
        };
        return match outcome {
            Ok(outcome) => Ok(scale_outcome_response(&$path_str, outcome)),
            Err(error) => {
                warn!(path = %$path_str, error = ?error, "rest::Scale update failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
            }
        };
    }
    }};
}
