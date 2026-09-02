    if is_get || is_list || is_create || is_delete || is_update {
        // Captured before `req` is potentially consumed below (`has_body`
        // moves it into `read_body_bytes`) — a borrow of `req.headers()`
        // can't outlive that move.
        let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        // Same reasoning — `GET`/`LIST`'s own `Table` negotiation
        // (`kubectl get`'s real default `Accept` header) needs this
        // after `req` may already be gone.
        let accepted = req.headers().get("accept").and_then(|v| v.to_str().ok()).and_then(negotiation::negotiate);
        let wants_table = accepted.as_ref().is_some_and(|a| a.wants_table());

        if let Some(mut client) = storage {
            let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };
            let crd_printer_columns = if wants_table {
                match rest::resolve_dynamic_resource(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                    Ok(Some(resolved)) => Some(resolved.additional_printer_columns),
                    Ok(None) => None,
                    Err(error) => {
                        warn!(path = %path_str, error = ?error, "table response: failed to resolve CRD printer columns");
                        None
                    }
                }
            } else {
                None
            };

            let dry_run = if is_create || is_update || is_delete {
                match dry_run_query(&query) {
                    Ok(value) => value,
                    Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
                }
            } else {
                false
            };

            // CREATE/UPDATE carry a full submitted object; DELETE carries
            // DeleteOptions. Read the request exactly once because hyper's
            // incoming body is single-consumer.
            let (mut body_value, delete_options) = if has_body || is_delete {
                let body_bytes = match read_body_bytes(req).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "reading the request body failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                };
                if is_delete {
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
                            Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error))),
                        }
                    }
                } else {
                    let format = content_type.as_deref().and_then(negotiation::content_type).unwrap_or(negotiation::Format::Json);
                    let decoded: Result<serde_json::Value, String> = match format {
                        negotiation::Format::Json => crate::codec::json::decode(&body_bytes).map_err(|e| e.to_string()),
                        negotiation::Format::Yaml => crate::codec::yaml::decode(&body_bytes).map_err(|e| e.to_string()),
                        negotiation::Format::Protobuf => match rest::decode_protobuf_request(&mut client, &info.api_group, &info.api_version, &info.resource, &body_bytes).await {
                            Ok(Some(value)) => Ok(value),
                            Ok(None) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                            Err(error) => Err(error.to_string()),
                        },
                    };
                    match decoded {
                        Ok(value) => (Some(value), None),
                        Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error))),
                    }
                }
            } else {
                (None, None)
            };

            // Group J: run the pure mutating admission registry before the
            // storage-backed admission stages. This preserves the existing
            // DefaultTolerationSeconds -> ServiceAccount defaulting order,
            // while making pure plugins extensible without another direct
            // listener call for each one.
            if let Some(body) = body_value.as_mut() {
                let operation = if is_create {
                    admission::attributes::Operation::Create
                } else {
                    admission::attributes::Operation::Update
                };
                let registry = admission::chain::MutatingRegistry::with_builtins();
                let mut request = admission::chain::Request {
                    operation,
                    group: &info.api_group,
                    resource: &info.resource,
                    subresource: &info.subresource,
                    namespace: &info.namespace,
                    name: &info.name,
                    object: body,
                };
                registry.run(&mut request);
            }

            // `ServiceAccount`'s validating and I/O-backed mutation step
            // follows the pure registry. Defaulting has already happened;
            // `quick_decision` now says whether a real ServiceAccount lookup
            // is needed to finish the plugin.
            if is_create {
                if let Some(pod) = body_value.as_mut() {
                    if admission::service_account::applies_to(&info.api_group, &info.resource, &info.subresource) {
                        match admission::service_account::quick_decision(pod, admission::attributes::Operation::Create) {
                            admission::service_account::Decision::Allow => {}
                            admission::service_account::Decision::Forbidden(msg) => {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
                            }
                            admission::service_account::Decision::NeedsServiceAccountLookup => {
                                let sa_name = pod.get("spec").and_then(|s| s.get("serviceAccountName")).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                                match rest::get(&mut client, None, "", "v1", "serviceaccounts", namespace, &sa_name).await {
                                    Ok(rest::GetOutcome::Found(sa)) => {
                                        admission::service_account::mutate_with_service_account(pod, &sa, || {
                                            let suffix: String = uuid::Uuid::new_v4().to_string().chars().take(5).collect();
                                            format!("{}{suffix}", admission::service_account::SERVICE_ACCOUNT_VOLUME_PREFIX)
                                        });
                                    }
                                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                                        return Ok(json_response(
                                            StatusCode::FORBIDDEN,
                                            &admission_forbidden_status(&path_str, &format!("error looking up service account {:?}/{sa_name:?}: not found", info.namespace)),
                                        ));
                                    }
                                    Err(e) => {
                                        warn!(path = %path_str, error = ?e, "admission: service account lookup failed");
                                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                                    }
                                }
                            }
                        }
                    }
                }
            }

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

            // Group J: `ResourceQuota` — validating, `CREATE` only, pods/
            // PVCs/services only (see `admission::resource_quota`'s own
            // doc comment for the full, honestly-named scope). Runs last
            // among the mutating-then-validating admission blocks above,
            // same relative position real upstream's own default plugin
            // order uses (quota checks the final, fully-defaulted/mutated
            // object) — placed after `LimitRanger`'s own defaulting, so a
            // container that only got its requests/limits from a
            // `LimitRange` default is still counted correctly here. Two
            // real I/O steps: list every existing object of the same kind
            // already in the namespace (to sum existing usage) and every
            // `ResourceQuota` in it.
            let quota_kind = if !is_create {
                None
            } else if admission::resource_quota::applies_to(admission::attributes::Operation::Create, &info.api_group, &info.resource, &info.subresource) {
                Some("pods")
            } else if admission::resource_quota::applies_to_pvc(admission::attributes::Operation::Create, &info.api_group, &info.resource, &info.subresource) {
                Some("persistentvolumeclaims")
            } else if admission::resource_quota::applies_to_service(admission::attributes::Operation::Create, &info.api_group, &info.resource, &info.subresource) {
                Some("services")
            } else {
                None
            };
            // Populated for the pod/PVC/service evaluators (the generic
            // object-count evaluator doesn't persist its own status.used
            // yet — a named follow-up), consumed after `rest::create`
            // actually succeeds below. Computing this here (before
            // creation) rather than re-listing after
            // is deliberate: it's the exact same existing-usage snapshot
            // `check_pod_create` just used to allow the request, so the
            // two stay consistent with each other.
            let mut quota_usage_updates: Vec<(String, std::collections::BTreeMap<String, crate::scheme::quantity::Quantity>)> = Vec::new();
            if let Some(list_resource) = quota_kind {
                if let Some(new_object) = body_value.as_ref() {
                    let existing = match rest::list(&mut client, None, "", "v1", list_resource, namespace, "", "", 0, "").await {
                        Ok(rest::ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
                        Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => Vec::new(),
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, resource = list_resource, "admission: listing existing objects for ResourceQuota failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    };
                    match rest::list(&mut client, None, "", "v1", "resourcequotas", namespace, "", "", 0, "").await {
                        Ok(rest::ListOutcome::Found(list)) => {
                            let quotas = list["items"].as_array().cloned().unwrap_or_default();
                            let denial = match list_resource {
                                "pods" => admission::resource_quota::check_pod_create(new_object, &existing, &quotas),
                                "persistentvolumeclaims" => admission::resource_quota::check_pvc_create(new_object, &existing, &quotas),
                                _ => admission::resource_quota::check_service_create(new_object, &existing, &quotas),
                            };
                            if let Some(denial) = denial {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &denial)));
                            }
                            quota_usage_updates = match list_resource {
                                "pods" => admission::resource_quota::usage_after_pod_create(new_object, &existing, &quotas),
                                "persistentvolumeclaims" => admission::resource_quota::usage_after_pvc_create(new_object, &existing, &quotas),
                                _ => admission::resource_quota::usage_after_service_create(new_object, &existing, &quotas),
                            };
                        }
                        Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {
                            // No `resourcequotas` known to this build —
                            // same "nothing to enforce" no-op as an empty
                            // list.
                        }
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "admission: listing resource quotas failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                }
            } else if is_create && !info.namespace.is_empty() {
                // Group J: `ResourceQuota`'s generic object-count
                // evaluator (`admission::resource_quota::check_object_count_create`'s
                // own doc comment) — runs for any namespaced resource
                // `CREATE` that isn't already covered by the pod/PVC/
                // service evaluators above (a real, deliberate skip, not
                // an oversight: those three already track their own
                // legacy bare-name object count). Safe to run
                // unconditionally: a namespace with no `ResourceQuota`
                // referencing this resource's `count/...` key has
                // nothing to enforce.
                let existing = match rest::list(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, "", "", 0, "").await {
                    Ok(rest::ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
                    Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => Vec::new(),
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "admission: listing existing objects for ResourceQuota's object-count check failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                };
                match rest::list(&mut client, None, "", "v1", "resourcequotas", namespace, "", "", 0, "").await {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let quotas = list["items"].as_array().cloned().unwrap_or_default();
                        if let Some(denial) = admission::resource_quota::check_object_count_create(&info.api_group, &info.resource, &existing, &quotas) {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &denial)));
                        }
                        quota_usage_updates = admission::resource_quota::usage_after_object_count_create(&info.api_group, &info.resource, &existing, &quotas);
                    }
                    Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "admission: listing resource quotas failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            }

            // Group J: storage-backed `ValidatingAdmissionPolicy` bindings.
            // Authorization must complete before admission, and CEL gets
            // the same candidate/old object pair that the write will use.
            if is_create || is_update || is_delete {
                let operation = if is_create {
                    "CREATE"
                } else if is_update {
                    "UPDATE"
                } else {
                    "DELETE"
                };
                let old_object = if is_update || is_delete {
                    match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                        Ok(rest::GetOutcome::Found(object)) => Some(object),
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission: reading the existing object for ValidatingAdmissionPolicy failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                } else {
                    None
                };
                match admission::policy_enforcement::validate(&mut client, operation, &info.api_group, &info.api_version, &info.resource, &info.subresource, &info.namespace, &info.name, body_value.as_ref(), old_object.as_ref(), dry_run, identity.as_ref()).await {
                    Ok(outcome) => {
                        record_admission_outcome(admission_metadata.as_ref(), &outcome);
                        if let Some(message) = outcome.denial {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                        }
                    }
                    Err(error) => {
                        warn!(path = %path_str, error = %error, "admission: ValidatingAdmissionPolicy evaluation failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            }

            // Group J: admission control, unconditional — see
            // `admission`'s own doc comment for why this plugin, unlike
            // Group I's RBAC, needs no config gate (it needs no
            // operator-provisioned bootstrap data, so there's no
            // "could lock every request out" risk). Only the three
            // mutating verbs pass through a real admission plugin at all;
            // GET/LIST are unaffected, matching real upstream (admission
            // only ever runs on write operations).
            if is_create || is_update || is_delete {
                let operation = if is_create {
                    admission::attributes::Operation::Create
                } else if is_update {
                    admission::attributes::Operation::Update
                } else {
                    admission::attributes::Operation::Delete
                };
                let admission_attrs = admission::attributes::Attributes { operation, group: &info.api_group, resource: &info.resource, namespace: &info.namespace, name: &info.name };

                match admission::namespace_lifecycle::quick_decision(&admission_attrs) {
                    admission::namespace_lifecycle::QuickDecision::Allow => {}
                    admission::namespace_lifecycle::QuickDecision::Forbidden(msg) => {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
                    }
                    admission::namespace_lifecycle::QuickDecision::NeedsNamespaceLookup => {
                        // `namespaces` is cluster-scoped — looked up by
                        // name with no parent namespace, same convention
                        // every other cluster-scoped `get` in this crate
                        // uses.
                        let namespace_phase = match rest::get(&mut client, None, "", "v1", "namespaces", None, &info.namespace).await {
                            Ok(rest::GetOutcome::Found(ns)) => Some(ns.get("status").and_then(|s| s.get("phase")).and_then(|p| p.as_str()).unwrap_or("").to_string()),
                            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                            Err(e) => {
                                warn!(path = %path_str, error = ?e, "admission: namespace lookup failed");
                                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                            }
                        };
                        match admission::namespace_lifecycle::decide(&admission_attrs, namespace_phase.as_deref()) {
                            admission::namespace_lifecycle::Decision::Allow => {}
                            admission::namespace_lifecycle::Decision::Forbidden(msg) => {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
                            }
                            admission::namespace_lifecycle::Decision::NamespaceNotFound(_) => {
                                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                            }
                        }
                    }
                }
            }

            // Group J: invoke configured mutating and validating webhooks
            // after built-in admission and authorization have produced the
            // candidate object, but before REST persists it. UPDATE and
            // DELETE need the current object as oldObject (and DELETE's
            // object); a missing object is left to REST to report NotFound.
            if is_create || is_update || is_delete {
                let old_object = if is_update || is_delete {
                    match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                        Ok(rest::GetOutcome::Found(object)) => Some(object),
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "admission: reading the existing object for webhooks failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                } else {
                    None
                };
                let webhook_object = body_value.clone().or_else(|| old_object.clone());
                if let Some(webhook_object) = webhook_object {
                    let operation = if is_create {
                        admission::attributes::Operation::Create
                    } else if is_update {
                        admission::attributes::Operation::Update
                    } else {
                        admission::attributes::Operation::Delete
                    };
                    match admission::webhook::admit(
                        &mut client,
                        operation,
                        &info.api_group,
                        &info.api_version,
                        &info.resource,
                        &info.subresource,
                        &info.namespace,
                        &info.name,
                        webhook_object,
                        old_object,
                        identity.as_ref(),
                        dry_run,
                    )
                    .await
                    {
                        Ok(admission::webhook::Outcome::Allowed(admitted)) => {
                            if is_create || is_update {
                                body_value = Some(admitted);
                            }
                        }
                        Ok(admission::webhook::Outcome::Denied(message)) => {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                        }
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission webhook invocation failed");
                            return Ok(admission_webhook_error_response(&path_str, &error));
                        }
                    }
                }
            }

            // Built-in resources have a real cache registered at startup;
            // dynamically discovered CRD resources are registered by the
            // CRD lifecycle reconciler and can still be registered lazily
            // by the first watch if startup discovery has not caught up.
            // Shared by both verbs below; `rest::list`'s own doc
            // comment covers why an unsynced cache is safe to pass here
            // too (it just falls through, same as `None`).
            let resource_cache = cache_registry.get(&info.api_group, &info.api_version, &info.resource);
            let resource_cache = resource_cache.as_ref();

            if is_get {
                match rest::get_at_revision(&mut client, resource_cache, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, resource_version_query(&query)).await {
                    Ok(rest::GetOutcome::Found(object)) => {
                        let body = if wants_table {
                            crate::codec::table::convert_to_table_for_resource_with_crd_columns(&info.api_group, &info.api_version, &info.resource, crd_printer_columns.as_deref(), &object)
                        } else if wants_partial_metadata {
                            crate::codec::partial_metadata::object(&object)
                        } else {
                            object
                        };
                        return Ok(json_response(StatusCode::OK, &body));
                    }
                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                    }
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::get failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            } else if is_list {
                if !info.field_selector.is_empty() {
                    match crate::cacher::selector::parse_field_selector(&info.field_selector) {
                        Ok(requirements) => {
                            if let Err(error) = crate::cacher::selector::validate_field_selector(&info.api_group, &info.resource, &requirements) {
                                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string())));
                            }
                        }
                        Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
                    }
                }
                match rest::list_at_revision(&mut client, resource_cache, &info.api_group, &info.api_version, &info.resource, namespace, &info.label_selector, &info.field_selector, info.limit, &info.continue_token, resource_version_query(&query)).await {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let body = if wants_table {
                            crate::codec::table::convert_to_table_for_resource_with_crd_columns(&info.api_group, &info.api_version, &info.resource, crd_printer_columns.as_deref(), &list)
                        } else if wants_partial_metadata {
                            crate::codec::partial_metadata::list(&list)
                        } else {
                            list
                        };
                        return Ok(json_response(StatusCode::OK, &body));
                    }
                    Ok(rest::ListOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                    Ok(rest::ListOutcome::InvalidContinueToken) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "continue token is not valid")));
                    }
                    // A malformed selector is the client's fault, not a
                    // server failure — real upstream answers this with a
                    // 400, not a 500.
                    Err(rest::Error::Selector(e)) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::list failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            } else if is_create {
                // `has_body` guarantees this is `Some` — the decode
                // happened above, before this branch was even chosen.
                let body_value = body_value.expect("body_value is Some whenever is_create is true (has_body covers it)");
                match rest::create_with_options_and_manager(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &body_value, dry_run, request_field_manager.as_deref()).await {
                    Ok(rest::CreateOutcome::Created(object)) => {
                        // Group J: persist `ResourceQuota.status.used` now
                        // that the object this usage total was computed
                        // for is genuinely real. Best-effort — a status
                        // write failing here must never turn an already-
                        // succeeded create into an error response; the
                        // request was correctly admitted regardless of
                        // whether its bookkeeping write lands.
                        if let Some(ns) = namespace {
                            persist_quota_usage_updates(&mut client, ns, quota_usage_updates, &path_str).await;
                        }
                        return Ok(json_response(StatusCode::CREATED, &object));
                    }
                    Ok(rest::CreateOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                    Ok(rest::CreateOutcome::MissingName) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.name or metadata.generateName is required")));
                    }
                    Ok(rest::CreateOutcome::NamespaceMismatch) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.namespace does not match the request URL")));
                    }
                    Ok(rest::CreateOutcome::AlreadyExists) => return Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
                    Ok(rest::CreateOutcome::Invalid(violations)) => return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                    Ok(rest::CreateOutcome::UnsupportedForCrd) => {
                        return Ok(json_response(StatusCode::NOT_IMPLEMENTED, &bad_request_status(&path_str, "this resource has no usable structural schema")));
                    }
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::create failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            } else if is_update {
                let body_value = body_value.expect("body_value is Some whenever is_update is true (has_body covers it)");
                match rest::update_with_options_and_manager(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, &body_value, dry_run, request_field_manager.as_deref()).await {
                    Ok(rest::UpdateOutcome::Updated(object)) => return Ok(json_response(StatusCode::OK, &object)),
                    Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                    }
                    Ok(rest::UpdateOutcome::MissingResourceVersion) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.resourceVersion is required for an update")));
                    }
                    Ok(rest::UpdateOutcome::NamespaceMismatch) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.namespace does not match the request URL")));
                    }
                    Ok(rest::UpdateOutcome::Conflict) => return Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
                    Ok(rest::UpdateOutcome::Invalid(violations)) => return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                    // `rest::update` never itself returns this -- it's
                    // `rest::patch`-only, checked before `rest::patch` is
                    // even called (see the `PATCH` branch above). Kept
                    // exhaustive rather than `unreachable!()`.
                    Ok(rest::UpdateOutcome::UnsupportedPatchType) => return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str))),
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::update failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            } else {
                // is_delete.
                let preconditions = match delete_preconditions(delete_options.as_ref()) {
                    Ok(value) => value,
                    Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
                };
                match rest::delete_with_options(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, preconditions.as_ref(), dry_run).await {
                    Ok(rest::DeleteOutcome::Deleted(object)) => return Ok(json_response(StatusCode::OK, &object)),
                    Ok(rest::DeleteOutcome::ObjectNotFound) | Ok(rest::DeleteOutcome::UnknownResource) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                    }
                    Ok(rest::DeleteOutcome::PreconditionFailed) => {
                        return Ok(json_response(StatusCode::CONFLICT, &precondition_failed_status(&path_str)));
                    }
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::delete failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            }
        }
        // No nodestore connection at all (failed at startup, or not yet
        // reconnected) — falls through to the echo stub below rather than
        // claiming a 503 for a request this build genuinely can't judge
        // "not found" vs. "unreachable" for yet.
    }

