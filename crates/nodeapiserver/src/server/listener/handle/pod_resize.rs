macro_rules! handle_pod_resize {
    ($req:ident, $storage:ident, $path_str:ident, $query:ident, $info:ident, $request_field_manager:ident) => {{
        if $info.is_resource_request
            && $info.api_group.is_empty()
            && $info.api_version == "v1"
            && $info.resource == "pods"
            && $info.subresource == "resize"
            && !$info.name.is_empty()
            && matches!($info.verb.as_str(), "get" | "update" | "patch")
        {
            if $info.namespace.is_empty() {
                return Ok(json_response(
                    StatusCode::BAD_REQUEST,
                    &bad_request_status(&$path_str, "Pod resize requires a namespace"),
                ));
            }
            let Some(mut client) = $storage else {
                return Ok(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &internal_error_status(&$path_str),
                ));
            };
            if $info.verb == "get" {
                return match rest::get_pod_resize(&mut client, &$info.namespace, &$info.name).await
                {
                    Ok(rest::GetOutcome::Found(object)) => Ok(json_response(StatusCode::OK, &object)),
                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                        Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)))
                    }
                    Err(error) => {
                        warn!(path = %$path_str, error = ?error, "rest::get_pod_resize failed");
                        Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
                    }
                };
            }

            let dry_run = match dry_run_query(&$query) {
                Ok(value) => value,
                Err(detail) => {
                    return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, detail)))
                }
            };
            let content_type = $req
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let kind_of_patch = if $info.verb == "patch" {
                match content_type.as_deref() {
                    Some(content_type) => match rest::patch_kind_for_content_type(content_type) {
                        Some(kind) => kind,
                        None => {
                            return Ok(json_response(
                                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                                &bad_request_status(&$path_str, "unsupported Content-Type for Pod resize PATCH"),
                            ))
                        }
                    },
                    None => match rest::default_patch_kind_for_request(&mut client, "", "v1", "pods").await {
                        Ok(Some(kind)) => kind,
                        Ok(None) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
                        Err(error) => {
                            warn!(path = %$path_str, error = ?error, "resolving Pod resize PATCH strategy failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                        }
                    },
                }
            } else {
                rest::PatchKind::Merge
            };
            let body_bytes = match read_body_bytes($req).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, "reading the Pod resize request failed");
                    return Ok(body_read_error_response(&$path_str, &error));
                }
            };
            let body: serde_json::Value = if $info.verb == "update"
                && content_type.as_deref().and_then(negotiation::content_type)
                    == Some(negotiation::Format::Yaml)
            {
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
            let outcome = if $info.verb == "update" {
                rest::update_pod_resize(
                    &mut client,
                    &$info.namespace,
                    &$info.name,
                    &body,
                    dry_run,
                    $request_field_manager.as_deref(),
                )
                .await
            } else {
                rest::patch_pod_resize(
                    &mut client,
                    &$info.namespace,
                    &$info.name,
                    kind_of_patch,
                    &body,
                    dry_run,
                    $request_field_manager.as_deref(),
                )
                .await
            };
            return match outcome {
                Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
                Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => {
                    Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)))
                }
                Ok(rest::UpdateOutcome::MissingResourceVersion) => {
                    Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "metadata.resourceVersion is required")))
                }
                Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &update_conflict_status(&$path_str))),
                Ok(rest::UpdateOutcome::NamespaceMismatch) => {
                    Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "metadata.namespace does not match the request URL")))
                }
                Ok(rest::UpdateOutcome::Invalid(violations)) => {
                    Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations)))
                }
                Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                    Ok(json_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, &bad_request_status(&$path_str, "unsupported Content-Type for Pod resize PATCH")))
                }
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, "Pod resize request failed");
                    Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
                }
            };
        }
    }};
}
