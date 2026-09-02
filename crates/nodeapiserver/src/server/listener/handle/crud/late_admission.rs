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
            // Populated for whichever evaluator applies, consumed after
            // `rest::create` actually succeeds below. Computing this here (before
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
