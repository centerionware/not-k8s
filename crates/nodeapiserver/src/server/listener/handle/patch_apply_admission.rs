macro_rules! handle_patch_apply_admission {
    (
        $client:ident, $candidate:ident, $apply_context:ident, $dry_run:ident,
        $old_object:ident, $operation:ident, $info:ident, $path_str:ident,
        $identity:ident, $admission_metadata:ident, $namespace:ident,
        $cache_registry:ident
    ) => {{
            // MutatingAdmissionPolicy is part of the same $candidate-based
            // admission chain as ordinary CREATE/UPDATE. Apply must not
            // bypass it merely because its field-management preparation
            // happens in a separate REST helper.
            let operation_name = match $operation {
                admission::attributes::Operation::Create => "CREATE",
                admission::attributes::Operation::Update => "UPDATE",
                _ => unreachable!("Server-Side Apply is create- or update-shaped"),
            };
            match admission::mutating_admission_policy::mutate(
                &mut $client,
                operation_name,
                &$info.api_group,
                &$info.api_version,
                &$info.resource,
                &$info.subresource,
                &$info.namespace,
                &$info.name,
                $candidate,
                $old_object.as_ref(),
                $dry_run,
                $identity.as_ref(),
                Some(&$cache_registry),
            )
            .await
            {
                Ok(admitted) => $candidate = admitted,
                Err(error) => {
                    warn!(path = %$path_str, error, "admission: MutatingAdmissionPolicy failed for apply");
                    return Ok(json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &internal_error_status(&$path_str),
                    ));
                }
            }

            // PodSecurity and ResourceQuota are validating stages after all
            // mutators, just as on ordinary CREATE. Keeping them here means
            // an Apply cannot bypass namespace policy or quota accounting
            // merely because its $candidate was produced by SSA.
            if $operation == admission::attributes::Operation::Create
                && admission::pod_security::applies_to(
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                    admission::attributes::Operation::Create,
                )
            {
                match rest::get(
                    &mut $client,
                    None,
                    "",
                    "v1",
                    "namespaces",
                    None,
                    &$info.namespace,
                )
                .await
                {
                    Ok(rest::GetOutcome::Found(namespace_object)) => {
                        let level = admission::pod_security::enforcement_level(&namespace_object);
                        let violations = admission::pod_security::validate(&$candidate, level);
                        if !violations.is_empty() {
                            return Ok(json_response(
                                StatusCode::FORBIDDEN,
                                &admission_forbidden_status(&$path_str, &violations.join("; ")),
                            ));
                        }
                    }
                    Ok(rest::GetOutcome::ObjectNotFound)
                    | Ok(rest::GetOutcome::UnknownResource) => {}
                    Err(error) => {
                        warn!(path = %$path_str, error = ?error, "admission: namespace lookup for PodSecurity apply failed");
                        return Ok(json_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &internal_error_status(&$path_str),
                        ));
                    }
                }
            }

            let mut quota_usage_updates: Vec<(
                String,
                std::collections::BTreeMap<String, crate::scheme::quantity::Quantity>,
            )> = Vec::new();
            if $operation == admission::attributes::Operation::Create {
                let quota_kind = if admission::resource_quota::applies_to(
                    admission::attributes::Operation::Create,
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                ) {
                    Some("pods")
                } else if admission::resource_quota::applies_to_pvc(
                    admission::attributes::Operation::Create,
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                ) {
                    Some("persistentvolumeclaims")
                } else if admission::resource_quota::applies_to_service(
                    admission::attributes::Operation::Create,
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                ) {
                    Some("services")
                } else if !$info.namespace.is_empty() {
                    Some($info.resource.as_str())
                } else {
                    None
                };

                if let Some(list_resource) = quota_kind {
                    let existing = match rest::list(
                        &mut $client,
                        None,
                        &$info.api_group,
                        &$info.api_version,
                        list_resource,
                        $namespace,
                        "",
                        "",
                        0,
                        "",
                    )
                    .await
                    {
                        Ok(rest::ListOutcome::Found(list)) => {
                            list["items"].as_array().cloned().unwrap_or_default()
                        }
                        Ok(rest::ListOutcome::UnknownResource)
                        | Ok(rest::ListOutcome::InvalidContinueToken) => Vec::new(),
                        Err(error) => {
                            warn!(path = %$path_str, error = ?error, "admission: listing existing objects for ResourceQuota Apply failed");
                            return Ok(json_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &internal_error_status(&$path_str),
                            ));
                        }
                    };
                    match rest::list(
                        &mut $client,
                        None,
                        "",
                        "v1",
                        "resourcequotas",
                        $namespace,
                        "",
                        "",
                        0,
                        "",
                    )
                    .await
                    {
                        Ok(rest::ListOutcome::Found(list)) => {
                            let quotas = list["items"].as_array().cloned().unwrap_or_default();
                            let denial = match list_resource {
                                "pods" => admission::resource_quota::check_pod_create(
                                    &$candidate,
                                    &existing,
                                    &quotas,
                                ),
                                "persistentvolumeclaims" => admission::resource_quota::check_pvc_create(
                                    &$candidate,
                                    &existing,
                                    &quotas,
                                ),
                                "services" => admission::resource_quota::check_service_create(
                                    &$candidate,
                                    &existing,
                                    &quotas,
                                ),
                                _ => admission::resource_quota::check_object_count_create(
                                    &$info.api_group,
                                    &$info.resource,
                                    &existing,
                                    &quotas,
                                ),
                            };
                            if let Some(denial) = denial {
                                return Ok(json_response(
                                    StatusCode::FORBIDDEN,
                                    &admission_forbidden_status(&$path_str, &denial),
                                ));
                            }
                            quota_usage_updates = match list_resource {
                                "pods" => admission::resource_quota::usage_after_pod_create(
                                    &$candidate,
                                    &existing,
                                    &quotas,
                                ),
                                "persistentvolumeclaims" => admission::resource_quota::usage_after_pvc_create(
                                    &$candidate,
                                    &existing,
                                    &quotas,
                                ),
                                "services" => admission::resource_quota::usage_after_service_create(
                                    &$candidate,
                                    &existing,
                                    &quotas,
                                ),
                                _ => admission::resource_quota::usage_after_object_count_create(
                                    &$info.api_group,
                                    &$info.resource,
                                    &existing,
                                    &quotas,
                                ),
                            };
                        }
                        Ok(rest::ListOutcome::UnknownResource)
                        | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                        Err(error) => {
                            warn!(path = %$path_str, error = ?error, "admission: listing ResourceQuotas for apply failed");
                            return Ok(json_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &internal_error_status(&$path_str),
                            ));
                        }
                    }
                }
            }

            match admission::policy_enforcement::validate(
                &mut $client,
                operation_name,
                &$info.api_group,
                &$info.api_version,
                &$info.resource,
                &$info.subresource,
                &$info.namespace,
                &$info.name,
                Some(&$candidate),
                $old_object.as_ref(),
                $dry_run,
                $identity.as_ref(),
                Some(&$cache_registry),
            )
            .await
            {
                Ok(outcome) => {
                    record_admission_outcome($admission_metadata.as_ref(), &outcome);
                    if let Some(message) = outcome.denial {
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&$path_str, &message),
                        ));
                    }
                }
                Err(error) => {
                    warn!(path = %$path_str, error, "admission: ValidatingAdmissionPolicy failed for apply");
                    return Ok(json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &internal_error_status(&$path_str),
                    ));
                }
            }
            match admission::webhook::admit(
                &mut $client,
                $operation,
                &$info.api_group,
                &$info.api_version,
                &$info.resource,
                &$info.subresource,
                &$info.namespace,
                &$info.name,
                $candidate.clone(),
                $old_object,
                $identity.as_ref(),
                $dry_run,
                Some(&$cache_registry),
            )
            .await
            {
                Ok(admission::webhook::Outcome::Allowed(admitted)) => $candidate = admitted,
                Ok(admission::webhook::Outcome::Denied(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &message)));
                }
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, "admission webhook invocation failed for apply");
                    return Ok(admission_webhook_error_response(&$path_str, &error));
                }
            }

            return match rest::apply_persist(&mut $client, &$info.api_group, &$info.api_version, &$info.resource, $namespace, $apply_context, $candidate, $dry_run).await {
                Ok(rest::ApplyOutcome::Applied(object)) => {
                    if $operation == admission::attributes::Operation::Create {
                        if let Some(ns) = $namespace {
                            persist_quota_usage_updates(&mut $client, ns, quota_usage_updates, &$path_str).await;
                        }
                    }
                    Ok(json_response(StatusCode::OK, &object))
                }
                Ok(rest::ApplyOutcome::NoOp(object)) => Ok(json_response(StatusCode::OK, &object)),
                Ok(rest::ApplyOutcome::UnknownResource) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
                Ok(rest::ApplyOutcome::UnsupportedForCrd) => {
                    Ok(json_response(StatusCode::NOT_IMPLEMENTED, &bad_request_status(&$path_str, "Server-Side Apply requires a usable structural schema")))
                }
                Ok(rest::ApplyOutcome::Conflict(conflicts)) => Ok(json_response(StatusCode::CONFLICT, &ssa_conflict_status(&$path_str, &conflicts))),
                Ok(rest::ApplyOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations))),
                Err(e) => {
                    warn!(path = %$path_str, error = ?e, "rest::apply_persist failed");
                    Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
                }
            };
    }};
}
