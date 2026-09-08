macro_rules! handle_status {
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
    // The generic `<resource>/status` subresource — and CSR `approval` —
    // have their own branch for
    // the same reason `PATCH` is: the request body here is the caller's
    // view of the *whole* object (typically a GET's own response,
    // status field modified), not a patch document, and only
    // `rest::update_status`'s narrower "replace `.status` only" write
    // applies, not the general five-verb block's `rest::update`. **No
    // Group J admission runs here, named honestly: every admission
    // plugin that ever applies to an `Update`-shaped write in this crate
    // (`namespace_lifecycle`'s Terminating-namespace check,
    // `LimitRanger`'s PVC-minimum check) is specific to a create/full
    // object write and has nothing meaningful to say about a status-only
    // replace, so there's nothing to wire here yet either — same
    // reasoning `deletecollection`'s own doc comment below already gives
    // for skipping the same two plugins.
    if $is_certificate_status_subresource
        && $info.verb == "get"
        && !$info.name.is_empty()
    {
        let Some(mut client) = $storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        };
        return match rest::get(&mut client, None, &$info.api_group, &$info.api_version, &$info.resource, None, &$info.name).await {
            Ok(rest::GetOutcome::Found(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
            Err(error) => {
                warn!(path = %$path_str, error = ?error, "rest::get CSR subresource failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
            }
        };
    }

    if $info.is_resource_request
        && $info.verb == "update"
        && !$info.name.is_empty()
        && ($info.subresource == "status" || $is_certificate_status_subresource)
    {
        let Some(mut client) = $storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        };
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
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string()))),
        };
        let namespace = storage_namespace(&$info);
        if $is_certificate_status_subresource {
            let old_object = match rest::get(&mut client, None, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name).await {
                Ok(rest::GetOutcome::Found(object)) => Some(object),
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, "admission: reading the CSR for certificate signer authorization failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            };
            let action = if $info.subresource == "approval" { "approve" } else { "sign" };
            match admission::certificate::validate_signer_update(
                &mut client,
                $enforce_rbac,
                $identity.as_ref(),
                action,
                old_object.as_ref(),
                Some(&body_value),
                &$info.subresource,
            )
            .await
            {
                Ok(()) => {}
                Err(admission::certificate::Error::Forbidden(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &message)));
                }
                Err(admission::certificate::Error::Lookup(error)) => {
                    warn!(path = %$path_str, error = %error, "admission: certificate signer authorization failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            }
        }
        if authz::node::node_name($identity.as_ref()).is_some() {
            let old_object = match rest::get(&mut client, None, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name).await {
                Ok(rest::GetOutcome::Found(object)) => Some(object),
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, "admission: reading the existing object for NodeRestriction status update failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            };
            match admission::node_restriction::validate(
                &mut client,
                $identity.as_ref(),
                admission::attributes::Operation::Update,
                &$info.api_group,
                &$info.resource,
                &$info.subresource,
                &$info.namespace,
                &$info.name,
                Some(&body_value),
                old_object.as_ref(),
            )
            .await
            {
                Ok(()) => {}
                Err(admission::node_restriction::Error::Forbidden(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &message)));
                }
                Err(admission::node_restriction::Error::Lookup(error)) => {
                    warn!(path = %$path_str, error = %error, "admission: NodeRestriction lookup failed for status update");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            }
        }
        return match rest::update_status_with_manager(&mut client, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name, &body_value, dry_run, $request_field_manager.as_deref()).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
            Ok(rest::UpdateOutcome::MissingResourceVersion) => {
                Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "metadata.resourceVersion is required for an update")))
            }
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &update_conflict_status(&$path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations))),
            // `rest::update_status` never itself returns these two -- it
            // does not check a body namespace, and `UnsupportedPatchType`
            // is `rest::patch`-only. Keep the match exhaustive rather than
            // turning a future implementation change into a panic.
            Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
            }
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "rest::update_status failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
            }
        };
    }
    // `PUT .../namespaces/{name}/finalize` — real upstream's own
    // `NamespaceFinalize` subresource: the *only* sanctioned way to
    // remove `spec.finalizers` (namespace-controller's own
    // `finalize_namespace()` calls exactly this). Recognized by
    // `server::path.rs`'s `NAMESPACE_SUBRESOURCES` for a while before
    // this branch existed to actually serve it -- every request routed
    // here 404'd, so a Namespace's `kubernetes` finalizer could never
    // actually be removed once #541/#559/#560 started correctly
    // deferring its deletion: content got cleaned up, but the namespace
    // itself sat "Terminating" forever. Live-captured:
    // "namespace-controller failed to remove the kubernetes finalizer
    // ... 404 ... /namespaces/<ns>/finalize". Same no-admission posture
    // as the `status` branch above; `rest::update_finalize` only ever
    // replaces `spec.finalizers`, nothing else in the object.
    if $info.is_resource_request
        && $info.verb == "update"
        && !$info.name.is_empty()
        && $info.subresource == "finalize"
    {
        let Some(mut client) = $storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        };
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
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string()))),
        };
        let namespace = storage_namespace(&$info);
        return match rest::update_finalize(&mut client, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name, &body_value, dry_run).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
            Ok(rest::UpdateOutcome::MissingResourceVersion) => {
                Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, "metadata.resourceVersion is required for an update")))
            }
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &update_conflict_status(&$path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations))),
            Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
            }
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "rest::update_finalize failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
            }
        };
    }
    // `PATCH .../status` — the patch counterpart to the `PUT` branch just
    // above, closing the "PUT-only" gap that branch's own doc comment
    // named. Same no-admission posture as the `PUT` branch (nothing
    // applicable exists for a status-only write); the only new outcome
    // to handle is `Invalid` (a malformed patch document), which
    // `update_status` never itself returns but `rest::patch_status` can.
    if $info.is_resource_request
        && $info.verb == "patch"
        && !$info.name.is_empty()
        && ($info.subresource == "status" || $is_certificate_status_subresource)
    {
        let content_type = $req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        let Some(mut client) = $storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
        };
        let dry_run = match dry_run_query(&$query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, detail))),
        };
        let kind_of_patch = match content_type.as_deref() {
            Some(content_type) => match rest::patch_kind_for_content_type(content_type) {
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
        if $is_certificate_status_subresource {
            // `validate_signer_update` needs the *merged* candidate object
            // to tell whether `.status.certificate`/`.status.conditions`
            // actually changed -- a PATCH request's body is a patch
            // document, not the whole object, so it cannot be passed
            // directly the way the `verb == "update"` branch above passes
            // its full-object body. Real callers (nodecontroller's
            // certificatesigningrequest-signing-controller) sign via a
            // `application/merge-patch+json` PATCH to `/status`, so
            // passing `None` here made every real signing PATCH forbidden
            // unconditionally (docs/APISERVER_E2E_FIX.md, "TLS bootstrap
            // client certificate kubeconfig"). `rest::patch_prepare`
            // already does exactly the read-and-apply-patch this needs
            // (its own doc comment: "the patch document can reference any
            // path, only the final write is restricted to `.status`") --
            // reuse it here for admission purposes; `patch_status_with_manager`
            // below independently redoes the same read+patch to persist,
            // the same two-phase precheck-then-persist shape the apply
            // and NodeRestriction paths in this file already use.
            let candidate = match rest::patch_prepare(&mut client, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name, kind_of_patch, &patch_doc).await {
                Ok(rest::PatchPrepareOutcome::Ready(candidate, _context)) => Some(candidate),
                Ok(rest::PatchPrepareOutcome::UnknownResource) | Ok(rest::PatchPrepareOutcome::ObjectNotFound) => None,
                Ok(rest::PatchPrepareOutcome::Invalid(violations)) => {
                    return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations)));
                }
                Err(e) => {
                    warn!(path = %$path_str, error = ?e, "admission: preparing the CSR status patch for certificate signer authorization failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            };
            let old_object = match rest::get(&mut client, None, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name).await {
                Ok(rest::GetOutcome::Found(object)) => Some(object),
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                Err(error) => {
                    warn!(path = %$path_str, error = ?error, "admission: reading the CSR for certificate signer authorization failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            };
            let action = if $info.subresource == "approval" { "approve" } else { "sign" };
            match admission::certificate::validate_signer_update(
                &mut client,
                $enforce_rbac,
                $identity.as_ref(),
                action,
                old_object.as_ref(),
                candidate.as_ref(),
                &$info.subresource,
            )
            .await
            {
                Ok(()) => {}
                Err(admission::certificate::Error::Forbidden(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&$path_str, &message)));
                }
                Err(admission::certificate::Error::Lookup(error)) => {
                    warn!(path = %$path_str, error = %error, "admission: certificate signer authorization failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
                }
            }
        }
        return match rest::patch_status_with_manager(&mut client, &$info.api_group, &$info.api_version, &$info.resource, namespace, &$info.name, kind_of_patch, &patch_doc, dry_run, $request_field_manager.as_deref()).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str))),
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &update_conflict_status(&$path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&$path_str, &violations))),
            // `rest::patch_status` never itself returns these three --
            // no client-submitted `resourceVersion` is required (the
            // object being patched is the one this same call just read,
            // same reasoning `patch_persist` already established), no
            // body namespace is ever checked, and `UnsupportedPatchType`
            // is pre-checked above before `rest::patch_status` is ever
            // called. Kept exhaustive rather than `unreachable!()`.
            Ok(rest::UpdateOutcome::MissingResourceVersion) | Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
            }
            Err(e) => {
                warn!(path = %$path_str, error = ?e, "rest::patch_status failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)))
            }
        };
    }
    }};
}
