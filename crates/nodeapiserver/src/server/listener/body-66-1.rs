    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let request_field_manager = field_manager_query(&query)
        .or_else(|| req.headers().get("user-agent").and_then(|value| value.to_str().ok()).map(str::to_string))
        .filter(|value| !value.is_empty());
    let admission_metadata = req.extensions().get::<SharedAdmissionMetadata>().cloned();

    if let Some(check_name) = path_str.strip_prefix('/').filter(|p| matches!(*p, "healthz" | "readyz" | "livez")) {
        let verbose = path::parse_query(&query).iter().any(|(k, _)| k == "verbose");
        let checks = healthz::run_checks(check_name, storage.is_some());
        let (status, body) = healthz::render(check_name, &checks, verbose);
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return Ok(Response::builder().status(code).header("Content-Type", "text/plain; charset=utf-8").header("X-Content-Type-Options", "nosniff").body(body_from_bytes(body.into_bytes())).unwrap());
    }

    if path_str == "/metrics" {
        return Ok(Response::builder().status(StatusCode::OK).header("Content-Type", "text/plain; version=0.0.4; charset=utf-8").body(body_from_bytes(metrics::render().into_bytes())).unwrap());
    }

    if method == "GET" || method == "HEAD" {
        let parts = path::split_path(&path_str);
        let accept_header = req.headers().get("accept").and_then(|v| v.to_str().ok());
        // Group K: only fetch CRDs for a request that could actually need
        // them — an `/apis`-prefixed path with 3 or fewer segments is
        // exactly `route_discovery`'s own three real `apis`-shaped
        // branches (`/apis`, `/apis/{group}`, `/apis/{group}/{version}`);
        // anything longer is a resource-shaped GET (`/apis/{group}/
        // {version}/namespaces/{ns}/{resource}/...`), which `route_discovery`
        // itself answers `NotApplicable` for and which the generic REST
        // dispatch further down handles instead — that path, by far the
        // hottest one in practice, never pays this extra `LIST`.
        let (crds, aggregated) = if parts.first().map(String::as_str) == Some("apis") && parts.len() <= 3 {
            match storage.clone() {
                Some(mut client) => {
                    let crds = match rest::list_all_crds(&mut client).await {
                        Ok(crds) => crate::apiextensions::registry::discoverable_resources(crds.iter()),
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "discovery: fetching CRDs for the dynamic resource merge failed");
                            Vec::new()
                        }
                    };
                    // Group L Phase 3: the same real gate, the same
                    // bounded cost — only paid for a discovery-shaped
                    // request, never the hot resource-request path.
                    // `discovery::merged_group_version_map`'s own doc
                    // comment covers why this is group-level only.
                    let aggregated = match aggregator::route::discoverable_group_versions(&mut client).await {
                        Ok(pairs) => pairs,
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "discovery: fetching aggregated APIServices for the dynamic group merge failed");
                            Vec::new()
                        }
                    };
                    (crds, aggregated)
                }
                None => (Vec::new(), Vec::new()),
            }
        } else {
            (Vec::new(), Vec::new())
        };
        match route_discovery(&parts, accept_header, &crds, &aggregated) {
            DiscoveryRoute::Found(doc) => return Ok(json_response_with_content_type(StatusCode::OK, &doc, discovery_content_type(&parts, accept_header))),
            DiscoveryRoute::FoundRaw(bytes) => {
                return Ok(Response::builder().status(StatusCode::OK).header("Content-Type", "application/json").body(body_from_bytes(bytes.to_vec())).unwrap());
            }
            DiscoveryRoute::NotFound => {
                // Group L Phase 3's own last named gap, closed: a real
                // `GET /apis/{group}/{version}` for an aggregated group
                // real upstream itself answers with a *live* fetch to
                // the backend's own discovery endpoint (`checkAPIService`'s
                // own discovery-check dial reused for real traffic too) —
                // this build had no compiled/CRD/discovery-merge answer
                // for that path at all (`discovery::merged_group_version_
                // map`'s own doc comment names this exact gap), so it's
                // the one real case where falling through to `aggregate_
                // proxy` on a `NotFound` (rather than the resource-shaped
                // dispatch's own early check) is correct: `route_discovery`
                // already ruled out every local answer, and `aggregated`
                // (fetched above, same real pre-flight-gated list
                // `aggregate_proxy` itself would recompute) is the one
                // remaining source of truth. Any other `NotFound` (a
                // genuinely unserved group/version) still falls through
                // to the real `404` below unchanged.
                if let Some((group, version)) = aggregated_discovery_group_version(&parts, &aggregated) {
                    if let Some(mut client) = storage.clone() {
                        if let Ok(Some(api_service)) = aggregator::route::resolve(&mut client, group, version).await {
                            return Ok(aggregate_proxy(req, &method, &api_service, client, &path_str, &query).await);
                        }
                    }
                }
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            DiscoveryRoute::NotApplicable => {}
        }
    }

    let info = path::parse(&method, &path_str, &query);

    // Keep authorization ahead of every admission and REST handler. The
    // earlier implementation performed this check only inside the generic
    // CRUD block, which let PATCH, status writes, deletecollection, and the
    // proxy/streaming branches reach their handlers without RBAC when
    // enforcement was enabled. Virtual review resources are intentionally
    // exempt: they answer questions about authorization rather than mutate
    // the resource named by the request.
    if should_run_local_authorization(&info, enforce_rbac, authorization_webhook_allowed) {
        let Some(client) = storage.as_mut() else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let allowed = match authz::request_allowed(client, identity.as_ref(), &info).await {
            Ok(allowed) => allowed,
            Err(error) => {
                warn!(path = %path_str, error = %error, "node/RBAC authorization failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        if !allowed {
            let user_name = identity.as_ref().map(|id| id.name.as_str()).unwrap_or(ANONYMOUS_USERNAME);
            return Ok(json_response(StatusCode::FORBIDDEN, &forbidden_status(&path_str, user_name)));
        }
    }

    // Group N: the core node and Service proxy subresources are ordinary
    // request/response relays.  Keep them ahead of the generic REST
    // branches below: `GET .../services/name/proxy` otherwise looks like a
    // normal GET with an unknown subresource and would be returned as a
    // bring-up-shaped error instead of reaching the selected backend.
    if info.is_resource_request
        && info.api_group.is_empty()
        && matches!(info.resource.as_str(), "nodes" | "services")
        && is_proxy_request(&info)
        && !info.name.is_empty()
        && matches!(method.as_str(), "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS")
    {
        return Ok(proxy_resource(req, storage, &info, &method, &path_str, &query, &identity, enforce_rbac, kubelet_tls).await);
    }

    // Group E's real resource verbs so far: single-object GET (`get`, not
    // `list`/`watch` — `path::parse` already tells those apart by an empty
    // `name`), LIST (`list`, no name), CREATE (`create`, no name — a POST
    // to the collection URL), single-object DELETE (`delete`, name
    // required — no name means `deletecollection`, now real too — see its
    // own dedicated branch below), and UPDATE (`update`, name
    // required — a PUT). The scheduler's core `pods/binding` and Pod
    // `pods/ephemeralcontainers` subresources are handled separately below;
    // the remaining subresources still fall through (see `rest`'s own doc
    // comment). Everything else still falls through to the RequestInfo echo
    // below. `storage` is only
    // ever consumed once (moved into `client` here), which is why all
    // five verbs share this one `if let` rather than each checking it
    // separately.
    let is_get = info.is_resource_request && info.verb == "get" && !info.name.is_empty() && info.subresource.is_empty();
    let is_list = info.is_resource_request && info.verb == "list" && info.name.is_empty() && info.subresource.is_empty();
    let is_create = info.is_resource_request && info.verb == "create" && info.name.is_empty() && info.subresource.is_empty();
    let is_delete = info.is_resource_request && info.verb == "delete" && !info.name.is_empty() && info.subresource.is_empty();
    let is_update = info.is_resource_request && info.verb == "update" && !info.name.is_empty() && info.subresource.is_empty();
    // `watch` (no name — `path::parse` already tells a namefull `watch`
    // apart, though real upstream's own single-resource watch form isn't
    // handled specially here either way today) is deliberately handled in
    // its own branch below, not folded into the five-verb block above:
    // unlike those five, it needs no request body, no `storage`/`client`
    // (it's served purely from an already-registered `cacher::CacheRegistry`
    // cache — see that branch's own doc comment), and produces a
    // streaming response rather than one JSON document.
    let is_watch = info.is_resource_request && info.verb == "watch" && info.subresource.is_empty();

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
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
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
            let dry_run = match dry_run_query(&query) {
                Ok(value) => value,
                Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
            };
            let content_type = req.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_string);
            let body_bytes = match read_body_bytes(req).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "reading the ephemeralcontainers request failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            let body: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
                Ok(body) => body,
                Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
            };
            let outcome = if info.verb == "update" {
                rest::update_ephemeral_containers(&mut client, &info.namespace, &info.name, &body, dry_run, request_field_manager.as_deref()).await
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
                rest::patch_ephemeral_containers(&mut client, &info.namespace, &info.name, kind_of_patch, &body, dry_run, request_field_manager.as_deref()).await
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
    if info.is_resource_request && info.verb == "patch" && !info.name.is_empty() && info.subresource.is_empty() {
        let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);

        // Server-Side Apply — its own branch, not folded into the
        // three-patch-kind block below: `rest::patch_kind_for_content_type`
        // deliberately doesn't recognize this media type (its own doc
        // comment), the body is YAML (or JSON, a valid subset), and the
        // real orchestration (`rest::apply_prepare`/`apply_persist`,
        // Group G's `updater::apply` wired to storage) is a wholly
        // different code path from the three-patch-kind `rest::
        // patch_prepare`/`patch_persist` split above -- but the *same
        // shape* of split, for the same reason: so both
        // `namespace_lifecycle` and `LimitRanger` admission can run
        // against the real candidate object in between, matching the
        // three-patch-kind branch's own coverage exactly. **Named,
        // The runtime-schema CRD path is handled by the same orchestration;
        // schema-less legacy CRD records remain a defensive 501 outcome.
        if content_type.as_deref().map(is_apply_patch_content_type).unwrap_or(false) {
            let Some(mut client) = storage else {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            };
            let Some(manager) = field_manager_query(&query) else {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "the fieldManager query parameter is required for Server-Side Apply")));
            };
            let force = force_query(&query);
            let dry_run = match dry_run_query(&query) {
                Ok(value) => value,
                Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
            };
            let body_bytes = match read_body_bytes(req).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "reading the request body failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            let config: serde_json::Value = match crate::codec::yaml::decode(&body_bytes) {
                Ok(v) => v,
                Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
            };
            let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };

            // Group J: `namespace_lifecycle`, same `Update`-shaped check
            // every other write-shaped verb gets.
            let admission_attrs = admission::attributes::Attributes { operation: admission::attributes::Operation::Update, group: &info.api_group, resource: &info.resource, namespace: &info.namespace, name: &info.name };
            match admission::namespace_lifecycle::quick_decision(&admission_attrs) {
                admission::namespace_lifecycle::QuickDecision::Allow => {}
                admission::namespace_lifecycle::QuickDecision::Forbidden(msg) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
                }
                admission::namespace_lifecycle::QuickDecision::NeedsNamespaceLookup => {
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

            let (mut candidate, apply_context) = match rest::apply_prepare(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, &manager, force, &config).await {
                Ok(rest::ApplyPrepareOutcome::Ready(candidate, context)) => (candidate, context),
                Ok(rest::ApplyPrepareOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Ok(rest::ApplyPrepareOutcome::UnsupportedForCrd) => {
                    return Ok(json_response(StatusCode::NOT_IMPLEMENTED, &bad_request_status(&path_str, "Server-Side Apply requires a usable structural schema")));
                }
                Ok(rest::ApplyPrepareOutcome::Conflict(conflicts)) => return Ok(json_response(StatusCode::CONFLICT, &ssa_conflict_status(&path_str, &conflicts))),
                Ok(rest::ApplyPrepareOutcome::Invalid(violations)) => return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                Ok(rest::ApplyPrepareOutcome::NoOp(object)) => return Ok(json_response(StatusCode::OK, &object)),
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "rest::apply_prepare failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };

            // Group J: `LimitRanger`'s own PVC-`Update` validation — the
            // same real candidate object this build's own three-patch-
            // kind `PATCH` branch below already gates the same way (its
            // own comment covers why this is PVC-only).
            if admission::limit_ranger::applies_to(admission::attributes::Operation::Update, &info.api_group, &info.resource, "") {
                match rest::list(&mut client, None, "", "v1", "limitranges", namespace, "", "", 0, "").await {
                    Ok(rest::ListOutcome::Found(list)) => {
                        for limit_range in list["items"].as_array().cloned().unwrap_or_default() {
                            let errs = admission::limit_ranger::validate_pvc(&limit_range, &candidate);
                            if !errs.is_empty() {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &errs.join("; "))));
                            }
                        }
                    }
                    Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "admission: listing limit ranges failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            }

            let old_object = match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                Ok(rest::GetOutcome::Found(object)) => Some(object),
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "admission: reading the existing object for apply webhooks failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            let operation = if old_object.is_some() {
                admission::attributes::Operation::Update
            } else {
                admission::attributes::Operation::Create
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
                candidate.clone(),
                old_object,
                identity.as_ref(),
                dry_run,
            )
            .await
            {
                Ok(admission::webhook::Outcome::Allowed(admitted)) => candidate = admitted,
                Ok(admission::webhook::Outcome::Denied(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                }
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "admission webhook invocation failed for apply");
                    return Ok(admission_webhook_error_response(&path_str, &error));
                }
            }

            return match rest::apply_persist(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, apply_context, candidate, dry_run).await {
                Ok(rest::ApplyOutcome::Applied(object)) => Ok(json_response(StatusCode::OK, &object)),
                Ok(rest::ApplyOutcome::NoOp(object)) => Ok(json_response(StatusCode::OK, &object)),
                Ok(rest::ApplyOutcome::UnknownResource) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Ok(rest::ApplyOutcome::UnsupportedForCrd) => {
                    Ok(json_response(StatusCode::NOT_IMPLEMENTED, &bad_request_status(&path_str, "Server-Side Apply requires a usable structural schema")))
                }
                Ok(rest::ApplyOutcome::Conflict(conflicts)) => Ok(json_response(StatusCode::CONFLICT, &ssa_conflict_status(&path_str, &conflicts))),
                Ok(rest::ApplyOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "rest::apply_persist failed");
                    Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
                }
            };
        }

        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let dry_run = match dry_run_query(&query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
        };
        let kind_of_patch = match content_type.as_deref() {
            Some(content_type) => match rest::patch_kind_for_content_type(content_type) {
                Some(kind) => kind,
                None => {
                    return Ok(json_response(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        &bad_request_status(&path_str, "unsupported Content-Type for PATCH -- use application/json-patch+json, application/merge-patch+json, or application/strategic-merge-patch+json"),
                    ));
                }
            },
            None => match rest::default_patch_kind_for_request(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                Ok(Some(kind)) => kind,
                Ok(None) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "resolving the default PATCH strategy failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            },
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let patch_doc: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };

        // Group J: `namespace_lifecycle`, same `Update`-shaped check
        // `CREATE`/`UPDATE` already get (an "operation" of `Update` is
        // exactly right for a `PATCH` too — real upstream's own
        // `admission.Update` covers both).
        let admission_attrs = admission::attributes::Attributes { operation: admission::attributes::Operation::Update, group: &info.api_group, resource: &info.resource, namespace: &info.namespace, name: &info.name };
        match admission::namespace_lifecycle::quick_decision(&admission_attrs) {
            admission::namespace_lifecycle::QuickDecision::Allow => {}
            admission::namespace_lifecycle::QuickDecision::Forbidden(msg) => {
                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
            }
            admission::namespace_lifecycle::QuickDecision::NeedsNamespaceLookup => {
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

        let (mut candidate, context) = match rest::patch_prepare(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, kind_of_patch, &patch_doc).await {
            Ok(rest::PatchPrepareOutcome::Ready(candidate, context)) => (candidate, context),
            Ok(rest::PatchPrepareOutcome::UnknownResource) | Ok(rest::PatchPrepareOutcome::ObjectNotFound) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            Ok(rest::PatchPrepareOutcome::Invalid(violations)) => {
                return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations)));
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "rest::patch_prepare failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };

        // Group J: `LimitRanger`'s own PVC-`Update` validation — its only
        // `Update`-shaped check (pods are `CREATE`-only, real upstream's
        // own "containers are immutable after create" posture, see
        // `admission::limit_ranger::applies_to`'s own doc comment).
        if admission::limit_ranger::applies_to(admission::attributes::Operation::Update, &info.api_group, &info.resource, &info.subresource) {
            match rest::list(&mut client, None, "", "v1", "limitranges", namespace, "", "", 0, "").await {
                Ok(rest::ListOutcome::Found(list)) => {
                    for limit_range in list["items"].as_array().cloned().unwrap_or_default() {
                        let errs = admission::limit_ranger::validate_pvc(&limit_range, &candidate);
                        if !errs.is_empty() {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &errs.join("; "))));
                        }
                    }
                }
                Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "admission: listing limit ranges failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }
        }

        let old_object = match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
            Ok(rest::GetOutcome::Found(object)) => Some(object),
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "admission: reading the existing object for patch webhooks failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        match admission::webhook::admit(
            &mut client,
            admission::attributes::Operation::Update,
            &info.api_group,
            &info.api_version,
            &info.resource,
            &info.subresource,
            &info.namespace,
            &info.name,
            candidate.clone(),
            old_object,
            identity.as_ref(),
            dry_run,
        )
        .await
        {
            Ok(admission::webhook::Outcome::Allowed(admitted)) => candidate = admitted,
            Ok(admission::webhook::Outcome::Denied(message)) => {
                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "admission webhook invocation failed for patch");
                return Ok(admission_webhook_error_response(&path_str, &error));
            }
        }

        return match rest::patch_persist_with_manager(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, context, candidate, dry_run, request_field_manager.as_deref()).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
            // `rest::patch_persist` never itself returns these two -- a
            // submitted resourceVersion/namespace are `update`-only
            // outcomes, and `UnsupportedPatchType` is pre-checked before
            // `rest::patch_prepare` is ever called. Kept exhaustive rather
            // than `unreachable!()` so a future real use doesn't silently
            // panic in production.
            Ok(rest::UpdateOutcome::MissingResourceVersion) | Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "rest::patch_persist failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }
    // The generic `<resource>/status` subresource — its own branch for
    // the same reason `PATCH` is: the request body here is the caller's
    // view of the *whole* object (typically a GET's own response,
    // status field modified), not a patch document, and only
    // `rest::update_status`'s narrower "replace `.status` only" write
    // applies, not the general five-verb block's `rest::update`. **No
    // Group J admission runs here, named honestly**: every admission
    // plugin that ever applies to an `Update`-shaped write in this crate
    // (`namespace_lifecycle`'s Terminating-namespace check,
    // `LimitRanger`'s PVC-minimum check) is specific to a create/full
    // object write and has nothing meaningful to say about a status-only
    // replace, so there's nothing to wire here yet either — same
    // reasoning `deletecollection`'s own doc comment below already gives
    // for skipping the same two plugins.
    if info.is_resource_request && info.verb == "update" && !info.name.is_empty() && info.subresource == "status" {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let dry_run = match dry_run_query(&query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };
        return match rest::update_status_with_manager(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, &body_value, dry_run, request_field_manager.as_deref()).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
            Ok(rest::UpdateOutcome::MissingResourceVersion) => {
                Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.resourceVersion is required for an update")))
            }
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
            // `rest::update_status` never itself returns these two -- it
            // does not check a body namespace, and `UnsupportedPatchType`
            // is `rest::patch`-only. Keep the match exhaustive rather than
            // turning a future implementation change into a panic.
            Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "rest::update_status failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }
    // `PATCH .../status` — the patch counterpart to the `PUT` branch just
    // above, closing the "PUT-only" gap that branch's own doc comment
    // named. Same no-admission posture as the `PUT` branch (nothing
    // applicable exists for a status-only write); the only new outcome
    // to handle is `Invalid` (a malformed patch document), which
    // `update_status` never itself returns but `rest::patch_status` can.
    if info.is_resource_request && info.verb == "patch" && !info.name.is_empty() && info.subresource == "status" {
        let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let dry_run = match dry_run_query(&query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
        };
        let kind_of_patch = match content_type.as_deref() {
            Some(content_type) => match rest::patch_kind_for_content_type(content_type) {
                Some(kind) => kind,
                None => {
                    return Ok(json_response(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        &bad_request_status(&path_str, "unsupported Content-Type for PATCH -- use application/json-patch+json, application/merge-patch+json, or application/strategic-merge-patch+json"),
                    ));
                }
            },
            None => match rest::default_patch_kind_for_request(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                Ok(Some(kind)) => kind,
                Ok(None) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "resolving the default PATCH strategy failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            },
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let patch_doc: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };
        return match rest::patch_status_with_manager(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, kind_of_patch, &patch_doc, dry_run, request_field_manager.as_deref()).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
            // `rest::patch_status` never itself returns these three --
            // no client-submitted `resourceVersion` is required (the
            // object being patched is the one this same call just read,
            // same reasoning `patch_persist` already established), no
            // body namespace is ever checked, and `UnsupportedPatchType`
            // is pre-checked above before `rest::patch_status` is ever
            // called. Kept exhaustive rather than `unreachable!()`.
            Ok(rest::UpdateOutcome::MissingResourceVersion) | Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "rest::patch_status failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }
