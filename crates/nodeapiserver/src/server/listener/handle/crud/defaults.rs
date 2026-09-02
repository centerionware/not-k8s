            // Group J: `DefaultStorageClass` — mutating, `CREATE` only
            // (see `admission::default_storage_class`'s own doc comment).
            // Unlike `namespace_lifecycle`/`service_account`, this one has
            // no cheap `QuickDecision`-style early-out before the one real
            // I/O step: `mutate` itself checks whether the PVC already has
            // a class and no-ops, but only after the `StorageClass` list
            // has already been fetched — a real (small) inefficiency for
            // the common already-classed case, named honestly rather than
            // silently optimized around with a duplicated has-class check.
            if is_create {
                if let Some(pvc) = body_value.as_mut() {
                    if admission::default_storage_class::applies_to(&info.api_group, &info.resource, &info.subresource) {
                        match rest::list(&mut client, None, "storage.k8s.io", "v1", "storageclasses", None, "", "", 0, "").await {
                            Ok(rest::ListOutcome::Found(list)) => {
                                let classes = list["items"].as_array().cloned().unwrap_or_default();
                                admission::default_storage_class::mutate(pvc, &classes);
                            }
                            Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {
                                // This build's own discovery table doesn't
                                // know `storageclasses` at all — treat the
                                // same as "no default class exists" rather
                                // than failing the PVC create, matching
                                // upstream's own "no default class
                                // selected, do nothing" no-op path.
                            }
                            Err(e) => {
                                warn!(path = %path_str, error = ?e, "admission: listing storage classes failed");
                                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                            }
                        }
                    }
                }
            }

            // Group J: `DefaultIngressClass` — mutating, `CREATE` only.
            // Keep this after the pure mutators and before validators so
            // later admission sees the final Ingress candidate.
            if is_create {
                if let Some(ingress) = body_value.as_mut() {
                    if admission::default_ingress_class::applies_to(&info.api_group, &info.resource, &info.subresource) {
                        match rest::list(&mut client, None, "networking.k8s.io", "v1", "ingressclasses", None, "", "", 0, "").await {
                            Ok(rest::ListOutcome::Found(list)) => {
                                let classes = list["items"].as_array().cloned().unwrap_or_default();
                                admission::default_ingress_class::mutate(ingress, &classes);
                            }
                            Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                            Err(error) => {
                                warn!(path = %path_str, error = ?error, "admission: listing ingress classes failed");
                            }
                        }
                    }
                }
            }

            // Group J: `Priority` — resolve a Pod's named or global-default
            // PriorityClass on create, preserve the plugin-owned fields on
            // update, and reject competing global defaults.
            if is_create || is_update {
                let operation = if is_create {
                    admission::attributes::Operation::Create
                } else {
                    admission::attributes::Operation::Update
                };
                if let Some(object) = body_value.as_mut() {
                    if admission::priority::applies_to_pod(
                        operation,
                        &info.api_group,
                        &info.resource,
                        &info.subresource,
                    ) {
                        if is_update {
                            match rest::get(&mut client, None, "", "v1", "pods", namespace, &info.name).await {
                                Ok(rest::GetOutcome::Found(old_pod)) => {
                                    if let Err(error) = admission::priority::preserve_update_fields(object, &old_pod) {
                                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                                    }
                                }
                                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {}
                                Err(error) => {
                                    warn!(path = %path_str, error = ?error, "admission: reading the existing Pod for Priority failed");
                                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                                }
                            }
                        } else {
                            let class_name = object.pointer("/spec/priorityClassName").and_then(Value::as_str).unwrap_or("").to_string();
                            let named_class = if class_name.is_empty() {
                                None
                            } else {
                                match rest::get(&mut client, None, "scheduling.k8s.io", "v1", "priorityclasses", None, &class_name).await {
                                    Ok(rest::GetOutcome::Found(priority_class)) => Some(priority_class),
                                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &format!("no PriorityClass with name {class_name} was found"))));
                                    }
                                    Err(error) => {
                                        warn!(path = %path_str, error = ?error, "admission: PriorityClass lookup failed");
                                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                                    }
                                }
                            };
                            let default_class = if named_class.is_none() {
                                match rest::list(&mut client, None, "scheduling.k8s.io", "v1", "priorityclasses", None, "", "", 0, "").await {
                                    Ok(rest::ListOutcome::Found(list)) => {
                                        let classes = list["items"].as_array().cloned().unwrap_or_default();
                                        admission::priority::select_default(&classes).cloned()
                                    }
                                    Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => None,
                                    Err(error) => {
                                        warn!(path = %path_str, error = ?error, "admission: listing PriorityClasses failed");
                                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                                    }
                                }
                            } else {
                                None
                            };
                            if let Err(error) = admission::priority::mutate_pod(object, named_class.as_ref(), default_class.as_ref()) {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                            }
                        }
                    } else if admission::priority::applies_to_priority_class(
                        operation,
                        &info.api_group,
                        &info.resource,
                        &info.subresource,
                    ) && object.pointer("/globalDefault").and_then(Value::as_bool) == Some(true)
                    {
                        match rest::list(&mut client, None, "scheduling.k8s.io", "v1", "priorityclasses", None, "", "", 0, "").await {
                            Ok(rest::ListOutcome::Found(list)) => {
                                let existing = list["items"].as_array().cloned().unwrap_or_default();
                                if let Some(error) = admission::priority::validate_priority_class(object, &existing) {
                                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                                }
                            }
                            Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                            Err(error) => {
                                warn!(path = %path_str, error = ?error, "admission: listing PriorityClasses for validation failed");
                                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                            }
                        }
                    }
                }
            }

            // Group J: `RuntimeClass` — mutating and validating, `CREATE`
            // only for ordinary Pods. The RuntimeClass plugin's informer
            // lookup is represented by this live read; the pure module owns
            // the same overhead validation/defaulting and scheduling merge
            // once the object is available.
            if is_create
                && admission::runtime_class::applies_to(
                    admission::attributes::Operation::Create,
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                )
            {
                if let Some(pod) = body_value.as_mut() {
                    let runtime_class_name = pod
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
                                        &path_str,
                                        &format!(
                                            "pod rejected: RuntimeClass {runtime_class_name:?} not found"
                                        ),
                                    ),
                                ));
                            }
                            Err(error) => {
                                warn!(path = %path_str, error = ?error, "admission: RuntimeClass lookup failed");
                                return Ok(json_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    &internal_error_status(&path_str),
                                ));
                            }
                        }
                    } else {
                        None
                    };
                    if let Err(error) = admission::runtime_class::mutate_and_validate(pod, runtime_class.as_ref()) {
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&path_str, &error),
                        ));
                    }
                }
            }

            // Group J: `PodNodeSelector` — the namespace annotation form of
            // the upstream plugin. The annotation is an explicit opt-in, so
            // the live namespace read is harmless for ordinary namespaces.
            if is_create
                && admission::pod_node_selector::applies_to(
                    admission::attributes::Operation::Create,
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                )
            {
                if let Some(pod) = body_value.as_mut() {
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
                            warn!(path = %path_str, error = ?error, "admission: namespace lookup for PodNodeSelector failed");
                            return Ok(json_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &internal_error_status(&path_str),
                            ));
                        }
                    };
                    let selector = match admission::pod_node_selector::selector_for_namespace(
                        pod_node_selector_config.as_deref(),
                        namespace.unwrap_or(""),
                        annotation.as_deref(),
                    ) {
                        Ok(selector) => selector,
                        Err(error) => {
                            return Ok(json_response(
                                StatusCode::FORBIDDEN,
                                &admission_forbidden_status(&path_str, &error),
                            ));
                        }
                    };
                    if let Err(error) = admission::pod_node_selector::merge_namespace_selector(pod, &selector) {
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&path_str, &error),
                        ));
                    }
                }
            }

            // Group J: `LimitRanger` — mutating (pods only, `CREATE` only)
            // + validating (pods and PVCs; see
            // `admission::limit_ranger`'s own doc comment for exact scope
            // and what's not yet ported). `operation` mirrors the same
            // three-way mapping the other Group J blocks each compute
            // locally.
            {
                let operation = if is_create {
                    Some(admission::attributes::Operation::Create)
                } else if is_update {
                    Some(admission::attributes::Operation::Update)
                } else if is_delete {
                    Some(admission::attributes::Operation::Delete)
                } else {
                    None
                };
                if let Some(operation) = operation {
                    if admission::limit_ranger::applies_to(operation, &info.api_group, &info.resource, &info.subresource) {
                        match rest::list(&mut client, None, "", "v1", "limitranges", namespace, "", "", 0, "").await {
                            Ok(rest::ListOutcome::Found(list)) => {
                                let limit_ranges = list["items"].as_array().cloned().unwrap_or_default();
                                if let Some(body) = body_value.as_mut() {
                                    if is_create && info.resource == "pods" {
                                        admission::limit_ranger::mutate_pod(body, &limit_ranges);
                                    }
                                    for limit_range in &limit_ranges {
                                        let errs = if info.resource == "pods" {
                                            admission::limit_ranger::validate_pod(limit_range, body)
                                        } else {
                                            admission::limit_ranger::validate_pvc(limit_range, body)
                                        };
                                        if !errs.is_empty() {
                                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &errs.join("; "))));
                                        }
                                    }
                                }
                            }
                            Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {
                                // No `limitranges` known to this build at
                                // all — same "nothing to enforce" no-op as
                                // an empty list.
                            }
                            Err(e) => {
                                warn!(path = %path_str, error = ?e, "admission: listing limit ranges failed");
                                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                            }
                        }
                    }
                }
            }

            // Group J: `PersistentVolumeClaimResize` — a PVC expansion is
            // allowed only for a bound claim whose unchanged StorageClass
            // explicitly permits volume expansion.
            if is_update
                && admission::pvc_resize::applies_to(
                    admission::attributes::Operation::Update,
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                )
            {
                if let Some(candidate) = body_value.as_ref() {
                    let old_pvc = match rest::get(&mut client, None, "", "v1", "persistentvolumeclaims", namespace, &info.name).await {
                        Ok(rest::GetOutcome::Found(old_pvc)) => old_pvc,
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => Value::Null,
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission: reading the existing PVC for resize failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    };
                    match rest::list(&mut client, None, "storage.k8s.io", "v1", "storageclasses", None, "", "", 0, "").await {
                        Ok(rest::ListOutcome::Found(list)) => {
                            let classes = list["items"].as_array().cloned().unwrap_or_default();
                            if let Err(error) = admission::pvc_resize::validate_resize(candidate, &old_pvc, &classes) {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                            }
                        }
                        Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {
                            if let Err(error) = admission::pvc_resize::validate_resize(candidate, &old_pvc, &[]) {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                            }
                        }
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission: listing StorageClasses for PVC resize failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                }
            }

            // Group J: storage-backed `MutatingAdmissionPolicy` bindings.
            // Apply policy mutations after built-in mutators and before
            // built-in validators inspect or account for the final
            // candidate. UPDATE supplies the existing object as `oldObject`;
            // CREATE has no old object. The policy module also enforces the
            // admission-configuration exemptions required to avoid locking
            // the API server out of its own policy storage.
            if is_create || is_update {
                let old_object = if is_update {
                    match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                        Ok(rest::GetOutcome::Found(object)) => Some(object),
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                        Err(error) => {
                            warn!(path = %path_str, error = %error, "admission: reading the existing object for MutatingAdmissionPolicy failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                } else {
                    None
                };
                if let Some(candidate) = body_value.take() {
                    match admission::mutating_admission_policy::mutate(
                        &mut client,
                        if is_create { "CREATE" } else { "UPDATE" },
                        &info.api_group,
                        &info.api_version,
                        &info.resource,
                        &info.subresource,
                        &info.namespace,
                        &info.name,
                        candidate,
                        old_object.as_ref(),
                        dry_run,
                        identity.as_ref(),
                    )
                    .await
                    {
                        Ok(mutated) => body_value = Some(mutated),
                        Err(error) => {
                            warn!(path = %path_str, error = %error, "admission: MutatingAdmissionPolicy evaluation failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                }
            }

            // Group J: `PodSecurity` — validating, `CREATE` only (see
            // `admission::pod_security`'s own doc comment for exactly
            // which checks are ported and which are named, honest gaps).
            // The one real I/O step: fetch the target namespace to read
            // its own `pod-security.kubernetes.io/enforce` label.
            if is_create && admission::pod_security::applies_to(&info.api_group, &info.resource, &info.subresource, admission::attributes::Operation::Create) {
                if let Some(pod) = body_value.as_ref() {
                    match rest::get(&mut client, None, "", "v1", "namespaces", None, &info.namespace).await {
                        Ok(rest::GetOutcome::Found(ns)) => {
                            let level = admission::pod_security::enforcement_level(&ns);
                            let violations = admission::pod_security::validate(pod, level);
                            if !violations.is_empty() {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &violations.join("; "))));
                            }
                        }
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                            // No real namespace to read a label off —
                            // `namespace_lifecycle` is what's responsible
                            // for rejecting a create into a namespace that
                            // doesn't exist at all; this check just has
                            // nothing to enforce in that case.
                        }
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "admission: namespace lookup for PodSecurity failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                }
            }
