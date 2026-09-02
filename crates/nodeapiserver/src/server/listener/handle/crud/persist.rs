macro_rules! handle_crud_persist {
    (
        $client:ident, $body_value:ident, $delete_options:ident,
        $namespace:ident, $info:ident, $path_str:ident,
        $cache_registry:ident, $wants_table:ident, $wants_partial_metadata:ident,
        $crd_printer_columns:ident, $dry_run:ident, $request_field_manager:ident,
        $is_get:ident, $is_list:ident, $is_create:ident, $is_update:ident,
        $query:ident, $quota_usage_updates:ident
    ) => {{
            // Built-in resources have a real cache registered at startup;
            // dynamically discovered CRD resources are registered by the
            // CRD lifecycle reconciler and can still be registered lazily
            // by the first watch if startup discovery has not caught up.
            // Shared by both verbs below; `rest::list`'s own doc
            // comment covers why an unsynced cache is safe to pass here
            // too (it just falls through, same as `None`).
            let resource_cache = $cache_registry.get(&$info.api_group, &$info.api_version, &$info.resource);
            let resource_cache = resource_cache.as_ref();

            if $is_get {
                match rest::get_at_revision(&mut $client, resource_cache, &$info.api_group, &$info.api_version, &$info.resource, $namespace, &$info.name, resource_version_query(&$query)).await {
                    Ok(rest::GetOutcome::Found(object)) => {
                        let body = if $wants_table {
                            crate::codec::table::convert_to_table_for_resource_with_crd_columns(&$info.api_group, &$info.api_version, &$info.resource, $crd_printer_columns.as_deref(), &object)
                        } else if $wants_partial_metadata {
                            crate::codec::partial_metadata::object(&object)
                        } else {
                            object
                        };
                        return Ok(json_response(StatusCode::OK, &body));
                    }
                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
                    }
                    Err(e) => {
                        warn!(path = %$path_str, error = ?e, "rest::get failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                    }
                }
            } else if $is_list {
                match rest::list_at_revision(&mut $client, resource_cache, &$info.api_group, &$info.api_version, &$info.resource, $namespace, &$info.label_selector, &$info.field_selector, $info.limit, &$info.continue_token, resource_version_query(&$query)).await {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let body = if $wants_table {
                            crate::codec::table::convert_to_table_for_resource_with_crd_columns(&$info.api_group, &$info.api_version, &$info.resource, $crd_printer_columns.as_deref(), &list)
                        } else if $wants_partial_metadata {
                            crate::codec::partial_metadata::list(&list)
                        } else {
                            list
                        };
                        return Ok(json_response(StatusCode::OK, &body));
                    }
                    Ok(rest::ListOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
                    Ok(rest::ListOutcome::InvalidContinueToken) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "continue token is not valid")));
                    }
                    // A malformed selector is the $client's fault, not a
                    // server failure — real upstream answers this with a
                    // 400, not a 500.
                    Err(rest::Error::Selector(e)) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string()))),
                    Err(e) => {
                        warn!(path = %$path_str, error = ?e, "rest::list failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                    }
                }
            } else if $is_create {
                // `has_body` guarantees this is `Some` — the decode
                // happened above, before this branch was even chosen.
                let $body_value = $body_value.expect("$body_value is Some whenever $is_create is true (has_body covers it)");
                match rest::create_with_options_and_manager(&mut $client, &$info.api_group, &$info.api_version, &$info.resource, $namespace, &$body_value, $dry_run, $request_field_manager.as_deref()).await {
                    Ok(rest::CreateOutcome::Created(object)) => {
                        // Group J: persist `ResourceQuota.status.used` now
                        // that the object this usage total was computed
                        // for is genuinely real. Best-effort — a status
                        // write failing here must never turn an already-
                        // succeeded create into an error response; the
                        // request was correctly admitted regardless of
                        // whether its bookkeeping write lands.
                        if let Some(ns) = $namespace {
                            persist_quota_usage_updates(&mut $client, ns, $quota_usage_updates, &$path_str).await;
                        }
                        return Ok(json_response(StatusCode::CREATED, &object));
                    }
                    Ok(rest::CreateOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
                    Ok(rest::CreateOutcome::MissingName) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "metadata.name or metadata.generateName is required")));
                    }
                    Ok(rest::CreateOutcome::NamespaceMismatch) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "metadata.$namespace does not match the request URL")));
                    }
                    Ok(rest::CreateOutcome::AlreadyExists) => return Ok(json_response(StatusCode::CONFLICT, &conflict_status(&$path_str))),
                    Ok(rest::CreateOutcome::Invalid(violations)) => return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations))),
                    Ok(rest::CreateOutcome::UnsupportedForCrd) => {
                        return Ok(json_response(StatusCode::NOT_IMPLEMENTED, &bad_request_status(&$path_str, "this resource has no usable structural schema")));
                    }
                    Err(e) => {
                        warn!(path = %$path_str, error = ?e, "rest::create failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                    }
                }
            } else if $is_update {
                let $body_value = $body_value.expect("$body_value is Some whenever $is_update is true (has_body covers it)");
                match rest::update_with_options_and_manager(&mut $client, &$info.api_group, &$info.api_version, &$info.resource, $namespace, &$info.name, &$body_value, $dry_run, $request_field_manager.as_deref()).await {
                    Ok(rest::UpdateOutcome::Updated(object)) => return Ok(json_response(StatusCode::OK, &object)),
                    Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
                    }
                    Ok(rest::UpdateOutcome::MissingResourceVersion) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "metadata.resourceVersion is required for an update")));
                    }
                    Ok(rest::UpdateOutcome::NamespaceMismatch) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "metadata.$namespace does not match the request URL")));
                    }
                    Ok(rest::UpdateOutcome::Conflict) => return Ok(json_response(StatusCode::CONFLICT, &conflict_status(&$path_str))),
                    Ok(rest::UpdateOutcome::Invalid(violations)) => return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations))),
                    // `rest::update` never itself returns this -- it's
                    // `rest::patch`-only, checked before `rest::patch` is
                    // even called (see the `PATCH` branch above). Kept
                    // exhaustive rather than `unreachable!()`.
                    Ok(rest::UpdateOutcome::UnsupportedPatchType) => return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str))),
                    Err(e) => {
                        warn!(path = %$path_str, error = ?e, "rest::update failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                    }
                }
            } else {
                // is_delete.
                let preconditions = match delete_preconditions($delete_options.as_ref()) {
                    Ok(value) => value,
                    Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, detail))),
                };
                match rest::delete_with_options(&mut $client, &$info.api_group, &$info.api_version, &$info.resource, $namespace, &$info.name, preconditions.as_ref(), $dry_run).await {
                    Ok(rest::DeleteOutcome::Deleted(object)) => return Ok(json_response(StatusCode::OK, &object)),
                    Ok(rest::DeleteOutcome::ObjectNotFound) | Ok(rest::DeleteOutcome::UnknownResource) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
                    }
                    Ok(rest::DeleteOutcome::PreconditionFailed) => {
                        return Ok(json_response(StatusCode::CONFLICT, &precondition_failed_status(&$path_str)));
                    }
                    Err(e) => {
                        warn!(path = %$path_str, error = ?e, "rest::delete failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                    }
                }
            }
        }
        // No nodestore connection at all (failed at startup, or not yet
        // reconnected) — handled by the real unavailable/not-found response below.
    }};
}
