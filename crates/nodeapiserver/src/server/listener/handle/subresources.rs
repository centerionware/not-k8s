    // The scheduler binds a pending Pod through the real core
    // `pods/binding` subresource rather than replacing the whole Pod. This
    // must run before generic CRUD dispatch: `Binding` contains only the
    // selected Node and optional binding preconditions, while the REST
    // operation itself atomically updates the stored Pod.
    if info.is_resource_request
        && info.api_group.is_empty()
        && info.api_version == "v1"
        && info.resource == "pods"
        && info.subresource == "binding"
        && info.verb == "create"
        && !info.name.is_empty()
    {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        if info.namespace.is_empty() {
            return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "Pod binding requires a namespace")));
        }
        let body_bytes = match read_body_bytes(req).await {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(path = %path_str, error = ?error, "reading the Pod binding request failed");
                return Ok(body_read_error_response(&path_str, &error));
            }
        };
        let body: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(body) => body,
            Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
        };
        return match rest::bind_pod(&mut client, &info.namespace, &info.name, &body).await {
            Ok(rest::BindOutcome::Bound) => Ok(json_response(
                StatusCode::CREATED,
                &serde_json::json!({
                    "kind": "Status",
                    "apiVersion": "v1",
                    "metadata": {},
                    "status": "Success",
                    "code": 201,
                }),
            )),
            Ok(rest::BindOutcome::UnknownResource) | Ok(rest::BindOutcome::ObjectNotFound) => {
                Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)))
            }
            Ok(rest::BindOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &precondition_failed_status(&path_str))),
            Ok(rest::BindOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
            Err(error) => {
                warn!(path = %path_str, error = ?error, "rest::bind_pod failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }

    // The core Pod `ephemeralcontainers` subresource has its own update
    // strategy: GET returns the Pod, while PUT/PATCH may change only
    // `spec.ephemeralContainers`. The REST helpers reset every other field
    // and reject removal or mutation of an existing ephemeral container
    // before using the normal MVCC write path.
    if info.is_resource_request
        && info.api_group.is_empty()
        && info.api_version == "v1"
        && info.resource == "pods"
        && info.subresource == "ephemeralcontainers"
        && !info.name.is_empty()
    {
        if info.namespace.is_empty() {
            return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "Pod ephemeralcontainers requires a namespace")));
        }
        if info.verb == "get" {
            let Some(mut client) = storage else {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            };
            return match rest::get_ephemeral_containers(&mut client, &info.namespace, &info.name).await {
                Ok(rest::GetOutcome::Found(object)) => Ok(json_response(StatusCode::OK, &object)),
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "rest::get_ephemeral_containers failed");
                    Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
                }
            };
        }

        if info.verb == "update" || info.verb == "patch" {
            let Some(mut client) = storage else {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            };
            let existing_pod = match rest::get(&mut client, None, "", "v1", "pods", Some(&info.namespace), &info.name).await {
                Ok(rest::GetOutcome::Found(pod)) => pod,
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                    return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                }
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "admission: reading the Pod for ephemeralcontainers failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            let Some(service_account_name) = existing_pod
                .pointer("/spec/serviceAccountName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            else {
                return Ok(json_response(
                    StatusCode::FORBIDDEN,
                    &admission_forbidden_status(
                        &path_str,
                        &format!(
                            "no service account specified for pod {}/{}",
                            info.namespace, info.name
                        ),
                    ),
                ));
            };
            let service_account = match rest::get(
                &mut client,
                None,
                "",
                "v1",
                "serviceaccounts",
                Some(&info.namespace),
                service_account_name,
            )
            .await
            {
                Ok(rest::GetOutcome::Found(service_account)) => service_account,
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                    return Ok(json_response(
                        StatusCode::FORBIDDEN,
                        &admission_forbidden_status(
                            &path_str,
                            &format!(
                                "error looking up service account {}/{}: not found",
                                info.namespace, service_account_name
                            ),
                        ),
                    ));
                }
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "admission: reading the ServiceAccount for ephemeralcontainers failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            let dry_run = match dry_run_query(&query) {
                Ok(value) => value,
                Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
            };
            let content_type = req.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_string);
            let body_bytes = match read_body_bytes(req).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "reading the ephemeralcontainers request failed");
                    return Ok(body_read_error_response(&path_str, &error));
                }
            };
            let body: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
                Ok(body) => body,
                Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
            };
            let validate_ephemeral = |pod: &Value| {
                admission::service_account::validate_ephemeral_container_secret_references(
                    &service_account,
                    pod,
                )
                .map_err(|error| vec![error])
            };
            let outcome = if info.verb == "update" {
                rest::update_ephemeral_containers(&mut client, &info.namespace, &info.name, &body, dry_run, request_field_manager.as_deref(), validate_ephemeral).await
            } else {
                let kind_of_patch = match content_type.as_deref() {
                    Some(content_type) => match rest::patch_kind_for_content_type(content_type) {
                        Some(kind) => kind,
                        None => {
                            return Ok(json_response(
                                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                                &bad_request_status(&path_str, "unsupported Content-Type for the ephemeralcontainers subresource"),
                            ));
                        }
                    },
                    None => rest::PatchKind::StrategicMerge,
                };
                rest::patch_ephemeral_containers(&mut client, &info.namespace, &info.name, kind_of_patch, &body, dry_run, request_field_manager.as_deref(), validate_ephemeral).await
            };
            return match outcome {
                Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
                Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
                Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                Ok(rest::UpdateOutcome::MissingResourceVersion) | Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str))),
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "ephemeralcontainers update failed");
                    Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
                }
            };
        }
    }

    // `PATCH` is handled in its own branch, not folded into the five-verb
    // block below: its request body is a patch document, not a
    // full/partial object, and which of `rest::patch`'s three real patch
    // kinds applies is decided by `Content-Type` rather than the
    // JSON-vs-YAML negotiation `has_body` below uses. Group J admission
    // now runs on it too (`namespace_lifecycle` + `LimitRanger`'s own
    // PVC-update validation — the only two plugins that ever apply to an
    // `Update`-shaped write in this crate; every other Group J plugin is
    // `CREATE`-only, so there's nothing else to run here), via
    // `rest::patch_prepare`/`patch_persist`'s own split, which exists
    // specifically so admission can see the real candidate object in
    // between the two.
