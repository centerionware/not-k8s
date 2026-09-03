macro_rules! handle_patch_standard {
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
        $wants_partial_metadata:ident, $has_body:ident, $content_type:ident
    ) => {{
        let Some(mut client) = $storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        };
        let dry_run = match dry_run_query(&$query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, detail))),
        };
        let kind_of_patch = match $content_type.as_deref() {
            Some($content_type) => match rest::patch_kind_for_content_type($content_type) {
                Some(kind) => kind,
                None => {
                    return Ok(json_response(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        &bad_request_status(&$path_str, "unsupported Content-Type for PATCH -- use application/json-patch+json, application/merge-patch+json, or application/strategic-merge-patch+json"),
                    ));
                }
            },
            None => match rest::default_patch_kind_for_request(&mut client, &$info.api_group, &$info.api_version, &$info.resource).await {
                Ok(Some(kind)) => kind,
                Ok(None) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
                Err(e) => {
                    warn!(path = %$path_str, error = ?e, "resolving the default PATCH strategy failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            },
        };
        let body_bytes = match read_body_bytes($req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "reading the request body failed");
                return Ok(body_read_error_response(&$path_str, &e));
            }
        };
        let patch_doc: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string()))),
        };
        let namespace = storage_namespace(&$info);

        // Group J: `namespace_lifecycle`, same `Update`-shaped check
        // `CREATE`/`UPDATE` already get (an "operation" of `Update` is
        // exactly right for a `PATCH` too — real upstream's own
        // `admission.Update` covers both).
        let admission_attrs = admission::attributes::Attributes { operation: admission::attributes::Operation::Update, group: &$info.api_group, resource: &$info.resource, namespace: &$info.namespace, name: &$info.name };
        match admission::namespace_lifecycle::quick_decision(&admission_attrs) {
            admission::namespace_lifecycle::QuickDecision::Allow => {}
            admission::namespace_lifecycle::QuickDecision::Forbidden(msg) => {
                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &msg)));
            }
            admission::namespace_lifecycle::QuickDecision::NeedsNamespaceLookup => {
                let namespace_phase = match rest::get(&mut client, None, "", "v1", "namespaces", None, &$info.namespace).await {
                    Ok(rest::GetOutcome::Found(ns)) => Some(ns.get("status").and_then(|s| s.get("phase")).and_then(|p| p.as_str()).unwrap_or("").to_string()),
                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                    Err(e) => {
                        warn!(path = %$path_str, error = ?e, "admission: namespace lookup failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                    }
                };
                match admission::namespace_lifecycle::decide(&admission_attrs, namespace_phase.as_deref()) {
                    admission::namespace_lifecycle::Decision::Allow => {}
                    admission::namespace_lifecycle::Decision::Forbidden(msg) => {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &msg)));
                    }
                    admission::namespace_lifecycle::Decision::NamespaceNotFound(_) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
                    }
                }
            }
        }

        let (mut candidate, context) = match rest::patch_prepare(&mut client, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name, kind_of_patch, &patch_doc).await {
            Ok(rest::PatchPrepareOutcome::Ready(candidate, context)) => (candidate, context),
            Ok(rest::PatchPrepareOutcome::UnknownResource) | Ok(rest::PatchPrepareOutcome::ObjectNotFound) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
            }
            Ok(rest::PatchPrepareOutcome::Invalid(violations)) => {
                return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations)));
            }
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "rest::patch_prepare failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
        };

        // Group J: `LimitRanger`'s own PVC-`Update` validation — its only
        // `Update`-shaped check (pods are `CREATE`-only, real upstream's
        // own "containers are immutable after create" posture, see
        // `admission::limit_ranger::applies_to`'s own doc comment).
        if admission::limit_ranger::applies_to(admission::attributes::Operation::Update, &$info.api_group, &$info.resource, &$info.subresource) {
            match rest::list(&mut client, None, "", "v1", "limitranges", namespace, "", "", 0, "").await {
                Ok(rest::ListOutcome::Found(list)) => {
                    for limit_range in list["items"].as_array().cloned().unwrap_or_default() {
                        let errs = admission::limit_ranger::validate_pvc(&limit_range, &candidate);
                        if !errs.is_empty() {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &errs.join("; "))));
                        }
                    }
                }
                Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                Err(e) => {
                    warn!(path = %$path_str, error = ?e, "admission: listing limit ranges failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            }
        }

        // `PersistentVolumeClaimResize` also runs after a PATCH has been
        // materialized into an Update candidate. Re-read the old object so
        // the same bound-claim and StorageClass checks cover both PUT and
        // PATCH request shapes.
        if admission::pvc_resize::applies_to(
            admission::attributes::Operation::Update,
            &$info.api_group,
            &$info.resource,
            &$info.subresource,
        ) {
            let old_pvc = match rest::get(&mut client, None, "", "v1", "persistentvolumeclaims", namespace, &$info.name).await {
                Ok(rest::GetOutcome::Found(old_pvc)) => old_pvc,
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => Value::Null,
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, "admission: reading the existing PVC for patch resize failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            };
            match rest::list(&mut client, None, "storage.k8s.io", "v1", "storageclasses", None, "", "", 0, "").await {
                Ok(rest::ListOutcome::Found(list)) => {
                    let classes = list["items"].as_array().cloned().unwrap_or_default();
                    if let Err(error) = admission::pvc_resize::validate_resize(&candidate, &old_pvc, &classes) {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &error)));
                    }
                }
                Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {
                    if let Err(error) = admission::pvc_resize::validate_resize(&candidate, &old_pvc, &[]) {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &error)));
                    }
                }
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, "admission: listing StorageClasses for patch resize failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            }
        }

        let resource_cache = $cache_registry.get(&$info.api_group, &$info.api_version, &$info.resource);
        let old_object = match rest::get(&mut client, resource_cache.as_ref(), &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name).await {
            Ok(rest::GetOutcome::Found(object)) => Some(object),
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "admission: reading the existing object for patch webhooks failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            }
        };
        run_pure_admission(
            &$pure_admission,
            admission::attributes::Operation::Update,
            &$info,
            old_object.as_ref(),
            &mut candidate,
        );
        match admission::mutating_admission_policy::mutate(
            &mut client,
            "UPDATE",
            &$info.api_group,
            &$info.api_version,
            &$info.resource,
            &$info.subresource,
            &$info.namespace,
            &$info.name,
            candidate,
            old_object.as_ref(),
            dry_run,
            $identity.as_ref(),
            Some(&$cache_registry),
        )
        .await
        {
            Ok(admitted) => candidate = admitted,
            Err(error) => {
                warn!(path = %$path_str, error, "admission: MutatingAdmissionPolicy failed for patch");
                return Ok(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &internal_error_status(&$path_str),
                ));
            }
        }
        match admission::policy_enforcement::validate(
            &mut client,
            "UPDATE",
            &$info.api_group,
            &$info.api_version,
            &$info.resource,
            &$info.subresource,
            &$info.namespace,
            &$info.name,
            Some(&candidate),
            old_object.as_ref(),
            dry_run,
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
                warn!(path = %$path_str, error, "admission: ValidatingAdmissionPolicy failed for patch");
                return Ok(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &internal_error_status(&$path_str),
                ));
            }
        }
        if authz::node::node_name($identity.as_ref()).is_some() {
            match admission::node_restriction::validate(
                &mut client,
                $identity.as_ref(),
                admission::attributes::Operation::Update,
                &$info.api_group,
                &$info.resource,
                &$info.subresource,
                &$info.namespace,
                &$info.name,
                Some(&candidate),
                old_object.as_ref(),
            )
            .await
            {
                Ok(()) => {}
                Err(admission::node_restriction::Error::Forbidden(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &message)));
                }
                Err(admission::node_restriction::Error::Lookup(error)) => {
                    warn!(path = %$path_str, error = %error, "admission: NodeRestriction lookup failed for patch");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            }
        }
        match admission::webhook::admit(
            &mut client,
            admission::attributes::Operation::Update,
            &$info.api_group,
            &$info.api_version,
            &$info.resource,
            &$info.subresource,
            &$info.namespace,
            &$info.name,
            candidate.clone(),
            old_object,
            $identity.as_ref(),
            dry_run,
            Some(&$cache_registry),
        )
        .await
        {
            Ok(admission::webhook::Outcome::Allowed(admitted)) => candidate = admitted,
            Ok(admission::webhook::Outcome::Denied(message)) => {
                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &message)));
            }
            Err(error) => {
                warn!(path = %$path_str, error = ?error, "admission webhook invocation failed for patch");
                return Ok(admission_webhook_error_response(&$path_str, &error));
            }
        }

        return match rest::patch_persist_with_manager(&mut client, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name, context, candidate, dry_run, $request_field_manager.as_deref()).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &update_conflict_status(&$path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations))),
            // `rest::patch_persist` never itself returns these two -- a
            // submitted resourceVersion/namespace are `update`-only
            // outcomes, and `UnsupportedPatchType` is pre-checked before
            // `rest::patch_prepare` is ever called. Kept exhaustive rather
            // than `unreachable!()` so a future real use doesn't silently
            // panic in production.
            Ok(rest::UpdateOutcome::MissingResourceVersion) | Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
            }
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "rest::patch_persist failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
            }
        };
    }};
}
