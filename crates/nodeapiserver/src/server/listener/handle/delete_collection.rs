macro_rules! handle_delete_collection {
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
    // `deletecollection` is handled in its own branch too, for the same
    // reason `patch` is: it needs no request body at all (unlike
    // `create`/`update`), and has to validate each selected object before
    // deleting it. This mirrors upstream's DeleteCollection handler, which
    // passes its delete validator into the store and lets the store invoke
    // it for every matched object.
    if $info.is_resource_request && $info.verb == "deletecollection" && $info.subresource.is_empty() {
        let Some(mut client) = $storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        };
        let dry_run = match dry_run_query(&$query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, detail))),
        };
        let namespace = if $info.namespace.is_empty() { None } else { Some($info.namespace.as_str()) };
        let listed = match rest::list_delete_collection(&mut client, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.label_selector, &$info.field_selector).await {
            Ok(outcome) => outcome,
            Err(rest::Error::Selector(error)) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &error.to_string()))),
            Err(error) => {
                warn!(path = %$path_str, error = ?error, "rest::list_delete_collection failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
        };
        let rest::DeleteCollectionOutcome::Deleted(list) = listed else {
            return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
        };
        let items = list.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        for item in &items {
            let Some(name) = item.pointer("/metadata/name").and_then(Value::as_str) else {
                continue;
            };

            // DeleteCollection's admission attributes intentionally retain
            // an empty request name, as upstream does; the selected object
            // is still supplied as oldObject to policy/webhook admission.
            match admission::policy_enforcement::validate(
                &mut client,
                "DELETE",
                &$info.api_group,
                &$info.api_version,
                &$info.resource,
                &$info.subresource,
                &$info.namespace,
                "",
                None,
                Some(item),
                dry_run,
                $identity.as_ref(),
            )
            .await
            {
                Ok(outcome) => {
                    record_admission_outcome($admission_metadata.as_ref(), &outcome);
                    if let Some(message) = outcome.denial {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &message)));
                    }
                }
                Err(error) => {
                    warn!(path = %$path_str, error = %error, "admission: ValidatingAdmissionPolicy evaluation failed for deletecollection");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            }

            match admission::webhook::admit(
                &mut client,
                admission::attributes::Operation::Delete,
                &$info.api_group,
                &$info.api_version,
                &$info.resource,
                &$info.subresource,
                &$info.namespace,
                "",
                item.clone(),
                Some(item.clone()),
                $identity.as_ref(),
                dry_run,
            )
            .await
            {
                Ok(admission::webhook::Outcome::Allowed(_)) => {}
                Ok(admission::webhook::Outcome::Denied(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &message)));
                }
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, name, "admission webhook invocation failed for deletecollection");
                    return Ok(admission_webhook_error_response(&$path_str, &error));
                }
            }

            match rest::delete_with_options(&mut client, &$info.api_group, &$info.api_version, &$info.resource, namespace, name, None, dry_run).await {
                Ok(rest::DeleteOutcome::Deleted(_)) | Ok(rest::DeleteOutcome::ObjectNotFound) => {}
                Ok(rest::DeleteOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
                Ok(rest::DeleteOutcome::PreconditionFailed) => return Ok(json_response(StatusCode::CONFLICT, &precondition_failed_status(&$path_str))),
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, name, "rest::delete failed for deletecollection");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            }
        }
        return Ok(json_response(StatusCode::OK, &list));
    }
    }};
}
