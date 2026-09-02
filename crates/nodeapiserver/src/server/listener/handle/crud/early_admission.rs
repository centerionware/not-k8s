macro_rules! handle_crud_early {
    (
        $client:ident, $body_value:ident, $namespace:ident,
        $info:ident, $path_str:ident,
        $is_create:ident, $is_update:ident, $is_delete:ident,
        $identity:ident, $pure_admission:ident
    ) => {{
            // Group J: CertificateSubjectRestriction protects the built-in
            // apiserver-$client signer from a CSR requesting the
            // `system:masters` organization.
            if $is_create {
                match admission::certificate::validate_subject_restriction(
                    admission::attributes::Operation::Create,
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                    $body_value.as_ref(),
                ) {
                    Ok(()) => {}
                    Err(admission::certificate::Error::Forbidden(message)) => {
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&$path_str, &message),
                        ));
                    }
                    Err(admission::certificate::Error::Lookup(error)) => {
                        warn!(path = %$path_str, error = %error, "admission: certificate subject restriction failed");
                        return Ok(json_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &internal_error_status(&$path_str),
                        ));
                    }
                }
            }

            // Group I: the Node authorizer cannot inspect request bodies, so
            // NodeRestriction supplies the body-sensitive half of the same
            // upstream authorization chain. Fetch the old object only for a
            // node $identity and only when the operation needs it; ordinary
            // users and controller requests keep the existing hot path.
            if authz::node::node_name($identity.as_ref()).is_some() {
                let operation = if $is_create {
                    admission::attributes::Operation::Create
                } else if $is_update {
                    admission::attributes::Operation::Update
                } else {
                    admission::attributes::Operation::Delete
                };
                let old_object = if $is_update || $is_delete {
                    match rest::get(&mut $client, None, &$info.api_group, &$info.api_version, &$info.resource, $namespace, &$info.name).await {
                        Ok(rest::GetOutcome::Found(object)) => Some(object),
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                        Err(error) => {
                            warn!(path = %$path_str, error = ?error, "admission: reading the existing object for NodeRestriction failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                        }
                    }
                } else {
                    None
                };
                match admission::node_restriction::validate(
                    &mut $client,
                    $identity.as_ref(),
                    operation,
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                    &$info.$namespace,
                    &$info.name,
                    $body_value.as_ref(),
                    old_object.as_ref(),
                )
                .await
                {
                    Ok(()) => {}
                    Err(admission::node_restriction::Error::Forbidden(message)) => {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &message)));
                    }
                    Err(admission::node_restriction::Error::Lookup(error)) => {
                        warn!(path = %$path_str, error = %error, "admission: NodeRestriction lookup failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                    }
                }
            }

            // Group J: run the pure mutating admission registry before the
            // storage-backed admission stages. This preserves the existing
            // DefaultTolerationSeconds -> ServiceAccount defaulting order,
            // while making pure plugins extensible without another direct
            // listener call for each one.
            let old_object_for_admission = if $is_update {
                match rest::get(&mut $client, None, &$info.api_group, &$info.api_version, &$info.resource, $namespace, &$info.name).await {
                    Ok(rest::GetOutcome::Found(object)) => Some(object),
                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                    Err(error) => {
                        warn!(path = %$path_str, error = ?error, "admission: reading the existing object for pure plugins failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                    }
                }
            } else {
                None
            };
            if let Some(body) = $body_value.as_mut() {
                let operation = if $is_create {
                    admission::attributes::Operation::Create
                } else {
                    admission::attributes::Operation::Update
                };
                run_pure_admission(&$pure_admission, operation, &$info, old_object_for_admission.as_ref(), body);
            }

            // Group J: `StorageObjectInUseProtection` — mutating,
            // `CREATE` only. Add the standard PV/PVC/VAC protection
            // finalizer before any later admission stage observes the
            // candidate; nodecontroller removes it when deletion is safe.
            if $is_create {
                if let Some(body) = $body_value.as_mut() {
                    admission::storage_object_in_use_protection::mutate(
                        &$info.api_group,
                        &$info.resource,
                        &$info.subresource,
                        body,
                    );
                }
            }

            // `ServiceAccount`'s validating and I/O-backed mutation step
            // follows the pure registry. Defaulting has already happened;
            // `quick_decision` now says whether a real ServiceAccount lookup
            // is needed to finish the plugin.
            if $is_create {
                if let Some(pod) = $body_value.as_mut() {
                    if admission::service_account::applies_to(&$info.api_group, &$info.resource, &$info.subresource) {
                        match admission::service_account::quick_decision(pod, admission::attributes::Operation::Create) {
                            admission::service_account::Decision::Allow => {}
                            admission::service_account::Decision::Forbidden(msg) => {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &msg)));
                            }
                            admission::service_account::Decision::NeedsServiceAccountLookup => {
                                let sa_name = pod.get("spec").and_then(|s| s.get("serviceAccountName")).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                                match rest::get(&mut $client, None, "", "v1", "serviceaccounts", $namespace, &sa_name).await {
                                    Ok(rest::GetOutcome::Found(sa)) => {
                                        admission::service_account::mutate_with_service_account(pod, &sa, || {
                                            let suffix: String = uuid::Uuid::new_v4().to_string().chars().take(5).collect();
                                            format!("{}{suffix}", admission::service_account::SERVICE_ACCOUNT_VOLUME_PREFIX)
                                        });
                                        if let Err(error) = admission::service_account::validate_secret_references(&sa, pod) {
                                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &error)));
                                        }
                                    }
                                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                                        return Ok(json_response(
                                            StatusCode::FORBIDDEN,
                                            &admission_forbidden_status(&$path_str, &format!("error looking up service account {:?}/{sa_name:?}: not found", $info.$namespace)),
                                        ));
                                    }
                                    Err(e) => {
                                        warn!(path = %$path_str, error = ?e, "admission: service account lookup failed");
                                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                                    }
                                }
                            }
                        }
                    }
                }
            }
    }};
}
