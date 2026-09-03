macro_rules! handle_crud {
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
    if $is_get || $is_list || $is_create || $is_delete || $is_update {
        // Captured before `$req` is potentially consumed below (`$has_body`
        // moves it into `read_body_bytes`) — a borrow of `$req.headers()`
        // can't outlive that move.
        let content_type = $req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        // Same reasoning — `GET`/`LIST`'s own `Table` negotiation
        // (`kubectl get`'s real default `Accept` header) needs this
        // after `$req` may already be gone.
        let accepted = $req.headers().get("accept").and_then(|v| v.to_str().ok()).and_then(negotiation::negotiate);
        let wants_table = accepted.as_ref().is_some_and(|a| a.wants_table());

        if let Some(mut client) = $storage {
            let namespace = storage_namespace(&$info);
            // ResourceQuota admission derives usage from a live object list
            // and persists the object later in this same request. Hold the
            // process-local reservation lock across that whole sequence so
            // concurrent namespaced creates cannot both pass against the
            // same pre-create snapshot.
            let _quota_admission_guard = if $is_create && namespace.is_some() {
                Some(RESOURCE_QUOTA_ADMISSION_LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await)
            } else {
                None
            };
            let crd_printer_columns = if wants_table {
                match rest::resolve_dynamic_resource(&mut client, &$info.api_group, &$info.api_version, &$info.resource).await {
                    Ok(Some(resolved)) => Some(resolved.additional_printer_columns),
                    Ok(None) => None,
                    Err(error) => {
                        warn!(path = %$path_str, error = ?error, "table response: failed to resolve CRD printer columns");
                        None
                    }
                }
            } else {
                None
            };

            let dry_run = if $is_create || $is_update || $is_delete {
                match dry_run_query(&$query) {
                    Ok(value) => value,
                    Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, detail))),
                }
            } else {
                false
            };

            // CREATE/UPDATE carry a full submitted object; DELETE carries
            // DeleteOptions. Read the request exactly once because hyper's
            // incoming body is single-consumer.
            let (mut body_value, delete_options) = if $has_body || $is_delete {
                let body_bytes = match read_body_bytes($req).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(path = %$path_str, error = ?e, "reading the request body failed");
                        return Ok(body_read_error_response(&$path_str, &e));
                    }
                };
                if $is_delete {
                    if body_bytes.is_empty() {
                        (None, None)
                    } else {
                        let format = content_type.as_deref().and_then(negotiation::content_type).unwrap_or(negotiation::Format::Json);
                        let decoded = match format {
                            negotiation::Format::Json => crate::codec::json::decode(&body_bytes).map_err(|e| e.to_string()),
                            negotiation::Format::Yaml => crate::codec::yaml::decode(&body_bytes).map_err(|e| e.to_string()),
                            negotiation::Format::Protobuf => Err("protobuf DELETE options are not decoded yet".to_string()),
                        };
                        match decoded {
                            Ok(value) => (None, Some(value)),
                            Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &error))),
                        }
                    }
                } else {
                    let format = content_type.as_deref().and_then(negotiation::content_type).unwrap_or(negotiation::Format::Json);
                    let decoded: Result<serde_json::Value, String> = match format {
                        negotiation::Format::Json => crate::codec::json::decode(&body_bytes).map_err(|e| e.to_string()),
                        negotiation::Format::Yaml => crate::codec::yaml::decode(&body_bytes).map_err(|e| e.to_string()),
                        negotiation::Format::Protobuf => match rest::decode_protobuf_request(&mut client, &$info.api_group, &$info.api_version, &$info.resource, &body_bytes).await {
                            Ok(Some(value)) => Ok(value),
                            Ok(None) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
                            Err(error) => Err(error.to_string()),
                        },
                    };
                    match decoded {
                        Ok(value) => (Some(value), None),
                        Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &error))),
                    }
                }
            } else {
                (None, None)
            };

            handle_crud_early!(client, body_value, namespace, $info, $path_str, $is_create, $is_update, $is_delete, $identity, $pure_admission);
            handle_crud_defaults!(client, body_value, namespace, $info, $path_str, $is_create, $is_update, $is_delete, $identity, dry_run, $pod_node_selector_config, $cache_registry);
            let mut quota_usage_updates: Vec<(String, std::collections::BTreeMap<String, crate::scheme::quantity::Quantity>)> = Vec::new();
            handle_crud_late_admission!(client, body_value, namespace, $info, $path_str, $is_create, $is_update, $is_delete, $identity, dry_run, $admission_metadata, quota_usage_updates, $cache_registry);
            handle_crud_persist!(client, body_value, delete_options, namespace, $info, $path_str, $cache_registry, wants_table, $wants_partial_metadata, crd_printer_columns, dry_run, $request_field_manager, $is_get, $is_list, $is_create, $is_update, $query, quota_usage_updates);
        }
        }
    }};
}
