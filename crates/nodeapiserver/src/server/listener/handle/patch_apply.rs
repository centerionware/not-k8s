macro_rules! handle_patch_apply {
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
        $wants_partial_metadata:ident, $has_body:ident, $content_type:ident,
        $namespace:ident
    ) => {{
        if $content_type.as_deref().map(is_apply_patch_content_type).unwrap_or(false) {
            let Some(mut client) = $storage else {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
            };
            let Some(manager) = field_manager_query(&$query) else {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "the fieldManager $query parameter is required for Server-Side Apply")));
            };
            let force = force_query(&$query);
            let dry_run = match dry_run_query(&$query) {
                Ok(value) => value,
                Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, detail))),
            };
            let body_bytes = match read_body_bytes($req).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(path = %$path_str, error = ?e, "reading the request body failed");
                    return Ok(body_read_error_response(&$path_str, &e));
                }
            };
            let config: serde_json::Value = match crate::codec::yaml::decode(&body_bytes) {
                Ok(v) => v,
                Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string()))),
            };
            let namespace = storage_namespace(&$info);

            // Group J: `namespace_lifecycle`, same `Update`-shaped check
            // every other write-shaped verb gets.
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

            let (mut candidate, apply_context) = match rest::apply_prepare(&mut client, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name, &manager, force, &config).await {
                Ok(rest::ApplyPrepareOutcome::Ready(candidate, context)) => (candidate, context),
                Ok(rest::ApplyPrepareOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
                Ok(rest::ApplyPrepareOutcome::UnsupportedForCrd) => {
                    return Ok(json_response(StatusCode::NOT_IMPLEMENTED, &bad_request_status(&$path_str, "Server-Side Apply requires a usable structural schema")));
                }
                Ok(rest::ApplyPrepareOutcome::Conflict(conflicts)) => return Ok(json_response(StatusCode::CONFLICT, &ssa_conflict_status(&$path_str, &conflicts))),
                Ok(rest::ApplyPrepareOutcome::Invalid(violations)) => return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations))),
                Ok(rest::ApplyPrepareOutcome::NoOp(object)) => return Ok(json_response(StatusCode::OK, &object)),
                Err(e) => {
                    warn!(path = %$path_str, error = ?e, "rest::apply_prepare failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            };

            // Group J: `LimitRanger`'s own PVC-`Update` validation — the
            // same real candidate object this build's own three-patch-
            // kind `PATCH` branch below already gates the same way (its
            // own comment covers why this is PVC-only).
            if admission::limit_ranger::applies_to(admission::attributes::Operation::Update, &$info.api_group, &$info.resource, "") {
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

            let old_object = match rest::get(&mut client, None, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name).await {
                Ok(rest::GetOutcome::Found(object)) => Some(object),
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                Err(e) => {
                    warn!(path = %$path_str, error = ?e, "admission: reading the existing object for apply webhooks failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            };
            let operation = if old_object.is_some() {
                admission::attributes::Operation::Update
            } else {
                admission::attributes::Operation::Create
            };

            // Node authorization cannot inspect an apply candidate's body.
            // Run the same body-sensitive NodeRestriction check as ordinary
            // writes before any mutating admission changes the candidate.
            if authz::node::node_name($identity.as_ref()).is_some() {
                match admission::node_restriction::validate(
                    &mut client,
                    $identity.as_ref(),
                    operation,
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
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&$path_str, &message),
                        ));
                    }
                    Err(admission::node_restriction::Error::Lookup(error)) => {
                        warn!(path = %$path_str, error = %error, "admission: NodeRestriction lookup failed for apply");
                        return Ok(json_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &internal_error_status(&$path_str),
                        ));
                    }
                }
            }
            run_pure_admission(&$pure_admission, operation, &$info, old_object.as_ref(), &mut candidate);

            // Apply must run the $storage-backed DefaultStorageClass plugin
            // against the materialized candidate too. A PVC with no class
            // is otherwise persisted differently depending on whether its
            // creator used POST or Server-Side Apply.
            if operation == admission::attributes::Operation::Create
                && admission::default_storage_class::applies_to(
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                )
            {
                match rest::list(
                    &mut client,
                    None,
                    "storage.k8s.io",
                    "v1",
                    "storageclasses",
                    None,
                    "",
                    "",
                    0,
                    "",
                )
                .await
                {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let classes = list["items"].as_array().cloned().unwrap_or_default();
                        admission::default_storage_class::mutate(&mut candidate, &classes);
                    }
                    Ok(rest::ListOutcome::UnknownResource)
                    | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                    Err(error) => {
                        warn!(path = %$path_str, error = ?error, "admission: listing StorageClasses for apply failed");
                        return Ok(json_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &internal_error_status(&$path_str),
                        ));
                    }
                }
            }

            // StorageObjectInUseProtection is a pure create-time mutator,
            // but Apply still has to invoke it so PV/PVC/VAC objects do not
            // lose their protection finalizer merely because they were
            // submitted with Server-Side Apply.
            if operation == admission::attributes::Operation::Create {
                admission::storage_object_in_use_protection::mutate(
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                    &mut candidate,
                );
            }
            // Keep Priority admission consistent with ordinary Pod and
            // PriorityClass writes. Apply candidates must resolve named or
            // default PriorityClasses, preserve controller-owned fields on
            // update, and reject a second global default.
            if matches!(
                operation,
                admission::attributes::Operation::Create
                    | admission::attributes::Operation::Update
            ) {
                if admission::priority::applies_to_pod(
                    operation,
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                ) {
                    if operation == admission::attributes::Operation::Update {
                        if let Some(old_pod) = old_object.as_ref() {
                            if let Err(error) =
                                admission::priority::preserve_update_fields(&mut candidate, old_pod)
                            {
                                return Ok(json_response(
                                    StatusCode::FORBIDDEN,
                                    &admission_forbidden_status(&$path_str, &error),
                                ));
                            }
                        }
                    } else {
                        let class_name = candidate
                            .pointer("/spec/priorityClassName")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let named_class = if class_name.is_empty() {
                            None
                        } else {
                            match rest::get(
                                &mut client,
                                None,
                                "scheduling.k8s.io",
                                "v1",
                                "priorityclasses",
                                None,
                                &class_name,
                            )
                            .await
                            {
                                Ok(rest::GetOutcome::Found(priority_class)) => Some(priority_class),
                                Ok(rest::GetOutcome::ObjectNotFound)
                                | Ok(rest::GetOutcome::UnknownResource) => {
                                    return Ok(json_response(
                                        StatusCode::FORBIDDEN,
                                        &admission_forbidden_status(
                                            &$path_str,
                                            &format!(
                                                "no PriorityClass with name {class_name} was found"
                                            ),
                                        ),
                                    ));
                                }
                                Err(error) => {
                                    warn!(path = %$path_str, error = ?error, "admission: PriorityClass lookup for apply failed");
                                    return Ok(json_response(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        &internal_error_status(&$path_str),
                                    ));
                                }
                            }
                        };
                        let default_class = if named_class.is_none() {
                            match rest::list(
                                &mut client,
                                None,
                                "scheduling.k8s.io",
                                "v1",
                                "priorityclasses",
                                None,
                                "",
                                "",
                                0,
                                "",
                            )
                            .await
                            {
                                Ok(rest::ListOutcome::Found(list)) => {
                                    let classes = list["items"].as_array().cloned().unwrap_or_default();
                                    admission::priority::select_default(&classes).cloned()
                                }
                                Ok(rest::ListOutcome::UnknownResource)
                                | Ok(rest::ListOutcome::InvalidContinueToken) => None,
                                Err(error) => {
                                    warn!(path = %$path_str, error = ?error, "admission: listing PriorityClasses for apply failed");
                                    return Ok(json_response(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        &internal_error_status(&$path_str),
                                    ));
                                }
                            }
                        } else {
                            None
                        };
                        if let Err(error) = admission::priority::mutate_pod(
                            &mut candidate,
                            named_class.as_ref(),
                            default_class.as_ref(),
                        ) {
                            return Ok(json_response(
                                StatusCode::FORBIDDEN,
                                &admission_forbidden_status(&$path_str, &error),
                            ));
                        }
                    }
                } else if admission::priority::applies_to_priority_class(
                    operation,
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                ) && candidate.pointer("/globalDefault").and_then(Value::as_bool) == Some(true)
                {
                    match rest::list(
                        &mut client,
                        None,
                        "scheduling.k8s.io",
                        "v1",
                        "priorityclasses",
                        None,
                        "",
                        "",
                        0,
                        "",
                    )
                    .await
                    {
                        Ok(rest::ListOutcome::Found(list)) => {
                            let existing = list["items"].as_array().cloned().unwrap_or_default();
                            if let Some(error) =
                                admission::priority::validate_priority_class(&candidate, &existing)
                            {
                                return Ok(json_response(
                                    StatusCode::FORBIDDEN,
                                    &admission_forbidden_status(&$path_str, &error),
                                ));
                            }
                        }
                        Ok(rest::ListOutcome::UnknownResource)
                        | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                        Err(error) => {
                            warn!(path = %$path_str, error = ?error, "admission: listing PriorityClasses for apply validation failed");
                            return Ok(json_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &internal_error_status(&$path_str),
                            ));
                        }
                    }
                }
            }

            // RuntimeClass mutates and validates ordinary Pod creates. Apply
            // must resolve the same class before policy/webhook admission so
            // its overhead and scheduling fields are part of the candidate.
            if operation == admission::attributes::Operation::Create
                && admission::runtime_class::applies_to(
                    admission::attributes::Operation::Create,
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                )
            {
                let runtime_class_name = candidate
                    .pointer("/spec/runtimeClassName")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let runtime_class = if let Some(runtime_class_name) = runtime_class_name {
                    match rest::get(
                        &mut client,
                        None,
                        "node.k8s.io",
                        "v1",
                        "runtimeclasses",
                        None,
                        &runtime_class_name,
                    )
                    .await
                    {
                        Ok(rest::GetOutcome::Found(runtime_class)) => Some(runtime_class),
                        Ok(rest::GetOutcome::ObjectNotFound)
                        | Ok(rest::GetOutcome::UnknownResource) => {
                            return Ok(json_response(
                                StatusCode::FORBIDDEN,
                                &admission_forbidden_status(
                                    &$path_str,
                                    &format!(
                                        "pod rejected: RuntimeClass {runtime_class_name:?} not found"
                                    ),
                                ),
                            ));
                        }
                        Err(error) => {
                            warn!(path = %$path_str, error = ?error, "admission: RuntimeClass lookup for apply failed");
                            return Ok(json_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &internal_error_status(&$path_str),
                            ));
                        }
                    }
                } else {
                    None
                };
                if let Err(error) =
                    admission::runtime_class::mutate_and_validate(&mut candidate, runtime_class.as_ref())
                {
                    return Ok(json_response(
                        StatusCode::FORBIDDEN,
                        &admission_forbidden_status(&$path_str, &error),
                    ));
                }
            }

            // PodNodeSelector reads the target namespace annotation and
            // merges it into newly-created Pods. Apply must see the same
            // namespace policy before persistence.
            if operation == admission::attributes::Operation::Create
                && admission::pod_node_selector::applies_to(
                    admission::attributes::Operation::Create,
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                )
            {
                let annotation = match rest::get(
                    &mut client,
                    None,
                    "",
                    "v1",
                    "namespaces",
                    None,
                    namespace.unwrap_or(""),
                )
                .await
                {
                    Ok(rest::GetOutcome::Found(namespace_object)) => namespace_object
                            .pointer("/metadata/annotations/scheduler.alpha.kubernetes.io~1node-selector")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                    Err(error) => {
                        warn!(path = %$path_str, error = ?error, "admission: namespace lookup for PodNodeSelector apply failed");
                        return Ok(json_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &internal_error_status(&$path_str),
                        ));
                    }
                };
                let selector = match admission::pod_node_selector::selector_for_namespace(
                    $pod_node_selector_config.as_deref(),
                    namespace.unwrap_or(""),
                    annotation.as_deref(),
                ) {
                    Ok(selector) => selector,
                    Err(error) => {
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&$path_str, &error),
                        ));
                    }
                };
                if let Err(error) = admission::pod_node_selector::merge_namespace_selector(&mut candidate, &selector) {
                    return Ok(json_response(
                        StatusCode::FORBIDDEN,
                        &admission_forbidden_status(&$path_str, &error),
                    ));
                }
            }

            // Apply must also run DefaultIngressClass against the candidate.
            // Otherwise an Ingress created with POST and one created with
            // Server-Side Apply receive different class defaulting.
            if operation == admission::attributes::Operation::Create
                && admission::default_ingress_class::applies_to(
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                )
            {
                match rest::list(
                    &mut client,
                    None,
                    "networking.k8s.io",
                    "v1",
                    "ingressclasses",
                    None,
                    "",
                    "",
                    0,
                    "",
                )
                .await
                {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let classes = list["items"].as_array().cloned().unwrap_or_default();
                        admission::default_ingress_class::mutate(&mut candidate, &classes);
                    }
                    Ok(rest::ListOutcome::UnknownResource)
                    | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                    Err(error) => {
                        warn!(path = %$path_str, error = ?error, "admission: listing IngressClasses for apply failed");
                        return Ok(json_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &internal_error_status(&$path_str),
                        ));
                    }
                }
            }

            // The pure registry only supplies the default ServiceAccount
            // name. Complete the $storage-backed ServiceAccount plugin for
            // create-on-apply as well, so applied Pods receive the same
            // token-volume, automount, imagePullSecret, and secret-reference
            // handling as ordinary Pod CREATE.
            if operation == admission::attributes::Operation::Create
                && admission::service_account::applies_to(
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                )
            {
                match admission::service_account::quick_decision(
                    &candidate,
                    admission::attributes::Operation::Create,
                ) {
                    admission::service_account::Decision::Allow => {}
                    admission::service_account::Decision::Forbidden(message) => {
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&$path_str, &message),
                        ));
                    }
                    admission::service_account::Decision::NeedsServiceAccountLookup => {
                        let service_account_name = candidate
                            .pointer("/spec/serviceAccountName")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        match rest::get(
                            &mut client,
                            None,
                            "",
                            "v1",
                            "serviceaccounts",
                            namespace,
                            &service_account_name,
                        )
                        .await
                        {
                            Ok(rest::GetOutcome::Found(service_account)) => {
                                admission::service_account::mutate_with_service_account(
                                    &mut candidate,
                                    &service_account,
                                    || {
                                        let suffix: String = uuid::Uuid::new_v4()
                                            .to_string()
                                            .chars()
                                            .take(5)
                                            .collect();
                                        format!(
                                            "{}{suffix}",
                                            admission::service_account::SERVICE_ACCOUNT_VOLUME_PREFIX
                                        )
                                    },
                                );
                                if let Err(error) =
                                    admission::service_account::validate_secret_references(
                                        &service_account,
                                        &candidate,
                                    )
                                {
                                    return Ok(json_response(
                                        StatusCode::FORBIDDEN,
                                        &admission_forbidden_status(&$path_str, &error),
                                    ));
                                }
                            }
                            Ok(rest::GetOutcome::ObjectNotFound)
                            | Ok(rest::GetOutcome::UnknownResource) => {
                                return Ok(json_response(
                                    StatusCode::FORBIDDEN,
                                    &admission_forbidden_status(
                                        &$path_str,
                                        &format!(
                                            "error looking up service account {:?}/{:?}: not found",
                                            $info.namespace, service_account_name
                                        ),
                                    ),
                                ));
                            }
                            Err(error) => {
                                warn!(path = %$path_str, error = ?error, "admission: service account lookup for apply failed");
                                return Ok(json_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    &internal_error_status(&$path_str),
                                ));
                            }
                        }
                    }
                }
            }

            // LimitRanger must observe the same materialized candidate for
            // Apply as it does for ordinary CREATE/UPDATE. In particular,
            // Pod requests/limits supplied by a namespace LimitRange are
            // part of the object ResourceQuota evaluates later in the
            // admission chain.
            if admission::limit_ranger::applies_to(
                operation,
                &$info.api_group,
                &$info.resource,
                &$info.subresource,
            ) {
                match rest::list(
                    &mut client,
                    None,
                    "",
                    "v1",
                    "limitranges",
                    namespace,
                    "",
                    "",
                    0,
                    "",
                )
                .await
                {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let limit_ranges = list["items"].as_array().cloned().unwrap_or_default();
                        if operation == admission::attributes::Operation::Create
                            && $info.resource == "pods"
                        {
                            admission::limit_ranger::mutate_pod(&mut candidate, &limit_ranges);
                        }
                        for limit_range in &limit_ranges {
                            let errors = if $info.resource == "pods" {
                                admission::limit_ranger::validate_pod(limit_range, &candidate)
                            } else {
                                admission::limit_ranger::validate_pvc(limit_range, &candidate)
                            };
                            if !errors.is_empty() {
                                return Ok(json_response(
                                    StatusCode::FORBIDDEN,
                                    &admission_forbidden_status(&$path_str, &errors.join("; ")),
                                ));
                            }
                        }
                    }
                    Ok(rest::ListOutcome::UnknownResource)
                    | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                    Err(error) => {
                        warn!(path = %$path_str, error = ?error, "admission: listing limit ranges for apply failed");
                        return Ok(json_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &internal_error_status(&$path_str),
                        ));
                    }
                }
            }

            // PVC expansion is another update-shaped admission check. Apply
            // must use the same old object and StorageClass capability check
            // as PUT and the ordinary patch kinds.
            if operation == admission::attributes::Operation::Update
                && admission::pvc_resize::applies_to(
                    admission::attributes::Operation::Update,
                    &$info.api_group,
                    &$info.resource,
                    &$info.subresource,
                )
            {
                if let Some(old_pvc) = old_object.as_ref() {
                    match rest::list(
                        &mut client,
                        None,
                        "storage.k8s.io",
                        "v1",
                        "storageclasses",
                        None,
                        "",
                        "",
                        0,
                        "",
                    )
                    .await
                    {
                        Ok(rest::ListOutcome::Found(list)) => {
                            let classes = list["items"].as_array().cloned().unwrap_or_default();
                            if let Err(error) = admission::pvc_resize::validate_resize(
                                &candidate,
                                old_pvc,
                                &classes,
                            ) {
                                return Ok(json_response(
                                    StatusCode::FORBIDDEN,
                                    &admission_forbidden_status(&$path_str, &error),
                                ));
                            }
                        }
                        Ok(rest::ListOutcome::UnknownResource)
                        | Ok(rest::ListOutcome::InvalidContinueToken) => {
                            if let Err(error) = admission::pvc_resize::validate_resize(
                                &candidate,
                                old_pvc,
                                &[],
                            ) {
                                return Ok(json_response(
                                    StatusCode::FORBIDDEN,
                                    &admission_forbidden_status(&$path_str, &error),
                                ));
                            }
                        }
                        Err(error) => {
                            warn!(path = %$path_str, error = ?error, "admission: listing StorageClasses for PVC resize Apply failed");
                            return Ok(json_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &internal_error_status(&$path_str),
                            ));
                        }
                    }
                }
            }
            handle_patch_apply_admission!(
                client, candidate, apply_context, dry_run, old_object,
                operation, $info, $path_str, $identity, $admission_metadata,
                $namespace
            );
        }
    }};
}
