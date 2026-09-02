    // `deletecollection` is handled in its own branch too, for the same
    // reason `patch` is: it needs no request body at all (unlike
    // `create`/`update`), and has to validate each selected object before
    // deleting it. This mirrors upstream's DeleteCollection handler, which
    // passes its delete validator into the store and lets the store invoke
    // it for every matched object.
    if info.is_resource_request && info.verb == "deletecollection" && info.subresource.is_empty() {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };
        let listed = match rest::list_delete_collection(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.label_selector, &info.field_selector).await {
            Ok(outcome) => outcome,
            Err(rest::Error::Selector(error)) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
            Err(error) => {
                warn!(path = %path_str, error = ?error, "rest::list_delete_collection failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let rest::DeleteCollectionOutcome::Deleted(list) = listed else {
            return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
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
                &info.api_group,
                &info.api_version,
                &info.resource,
                &info.subresource,
                &info.namespace,
                "",
                None,
                Some(item),
                false,
                identity.as_ref(),
            )
            .await
            {
                Ok(outcome) => {
                    record_admission_outcome(admission_metadata.as_ref(), &outcome);
                    if let Some(message) = outcome.denial {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                    }
                }
                Err(error) => {
                    warn!(path = %path_str, error = %error, "admission: ValidatingAdmissionPolicy evaluation failed for deletecollection");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }

            match admission::webhook::admit(
                &mut client,
                admission::attributes::Operation::Delete,
                &info.api_group,
                &info.api_version,
                &info.resource,
                &info.subresource,
                &info.namespace,
                "",
                item.clone(),
                Some(item.clone()),
                identity.as_ref(),
                false,
            )
            .await
            {
                Ok(admission::webhook::Outcome::Allowed(_)) => {}
                Ok(admission::webhook::Outcome::Denied(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                }
                Err(error) => {
                    warn!(path = %path_str, error = ?error, name, "admission webhook invocation failed for deletecollection");
                    return Ok(admission_webhook_error_response(&path_str, &error));
                }
            }

            match rest::delete(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, name).await {
                Ok(rest::DeleteOutcome::Deleted(_)) | Ok(rest::DeleteOutcome::ObjectNotFound) => {}
                Ok(rest::DeleteOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Ok(rest::DeleteOutcome::PreconditionFailed) => return Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
                Err(error) => {
                    warn!(path = %path_str, error = ?error, name, "rest::delete failed for deletecollection");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }
        }
        return Ok(json_response(StatusCode::OK, &list));
    }
    // Group I: `SubjectAccessReview`/`SelfSubjectAccessReview`/
    // `LocalSubjectAccessReview` — its own branch, checked before the
    // generic `is_create` handling below, because it's a virtual
    // resource: real upstream never persists any of the three kinds to
    // storage (`pkg/registry/authorization/subjectaccessreview`'s own
    // synthetic REST connector), and letting this fall through to the
    // generic `rest::create` path would actually try to write one to
    // nodestore — a real, wrong side effect this early return prevents.
    // Unconditional, not gated by `enforce_rbac`: answering "would RBAC
    // allow this" is a read on the RBAC engine's own state, not itself
    // an enforcement decision.
    if info.is_resource_request
        && info.api_group == "authorization.k8s.io"
        && matches!(info.resource.as_str(), "subjectaccessreviews" | "selfsubjectaccessreviews" | "localsubjectaccessreviews")
        && info.verb == "create"
        && info.subresource.is_empty()
    {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
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
        let (fallback_user, fallback_groups): (&str, Vec<String>) = match &identity {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let spec = body_value.get("spec").cloned().unwrap_or_default();
        let mut review = match authz::sar::parse_spec(&spec, fallback_user, &fallback_groups) {
            Ok(r) => r,
            Err(msg) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &msg))),
        };
        // `LocalSubjectAccessReview`'s own real semantics: the namespace
        // is the URL's, not whatever (if anything) the submitted
        // `resourceAttributes.namespace` said — the same "the URL is
        // authoritative over the body" rule `rest::update`'s own
        // namespace-mismatch check already establishes elsewhere in this
        // crate, applied here as an override rather than a rejection
        // since a `LocalSubjectAccessReview` isn't required to name a
        // namespace in its body at all.
        if info.resource == "localsubjectaccessreviews" {
            review.namespace = info.namespace.clone();
        }
        // Non-resource rules are only ever granted via ClusterRoleBindings
        // in real RBAC too (a namespace-scoped RoleBinding can't grant a
        // non-resource-URL permission) -- resolving with an empty
        // namespace naturally restricts to just those, no separate branch
        // needed.
        let resolve_namespace = if review.is_resource { review.namespace.as_str() } else { "" };
        let resolved = authz::resolve::rules_for(&mut client, &review.user_name, &review.user_groups, resolve_namespace).await;
        let attrs = authz::rbac::RequestAttributes {
            is_resource_request: review.is_resource,
            verb: &review.verb,
            api_group: &review.group,
            resource: &review.resource,
            subresource: &review.subresource,
            name: &review.name,
            path: &review.path,
        };
        let allowed = authz::rbac::rules_allow(&attrs, &resolved.rules);
        let mut response_body = body_value;
        response_body["status"] = authz::sar::build_status(allowed);
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // `SelfSubjectRulesReview` — lists the caller's own resolved rules
    // for one namespace, rather than answering a single allow/deny
    // question. Same virtual-resource/no-persistence reasoning as the
    // branch above, its own separate branch only because the response
    // shape (`resourceRules`/`nonResourceRules`, not `allowed`) and
    // input (`spec.namespace`, no attributes to parse) are different
    // enough not to share code cleanly.
    if info.is_resource_request && info.api_group == "authorization.k8s.io" && info.resource == "selfsubjectrulesreviews" && info.verb == "create" && info.subresource.is_empty() {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
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
        let (user_name, user_groups): (&str, Vec<String>) = match &identity {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let review_namespace = body_value.pointer("/spec/namespace").and_then(serde_json::Value::as_str).unwrap_or("");
        let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, review_namespace).await;
        let mut response_body = body_value;
        response_body["status"] = authz::sar::build_rules_status(&resolved.rules, &resolved.errors);
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // `SelfSubjectReview` (`kubectl auth whoami`) — the simplest of this
    // crate's virtual resources: no storage, no RBAC, purely reflects
    // whatever identity `authn::x509` (or the real anonymous fallback)
    // already produced. Same "checked before generic `is_create`, never
    // persisted" reasoning as every other review kind above.
    if info.is_resource_request && info.api_group == "authentication.k8s.io" && info.resource == "selfsubjectreviews" && info.verb == "create" && info.subresource.is_empty() {
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
        let (username, uid, groups): (&str, Option<&str>, Vec<String>) = match &identity {
            Some(id) => (id.name.as_str(), id.uid.as_deref(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, None, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let mut response_body = body_value;
        response_body["status"] = crate::authn::self_review::build_status(username, uid, &groups);
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // Group H: TokenReview is the webhook endpoint nodelet uses when a pod
    // presents its projected ServiceAccount token. It is virtual, just like
    // the authorization review resources above, and must never be written to
    // nodestore.
    if info.is_resource_request
        && info.api_group == "authentication.k8s.io"
        && info.resource == "tokenreviews"
        && info.verb == "create"
        && info.subresource.is_empty()
    {
        if storage.is_none() {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        }
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the TokenReview body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let mut response_body: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let token = response_body.pointer("/spec/token").and_then(serde_json::Value::as_str).unwrap_or("");
        let authenticated = service_account_authenticator
            .as_deref()
            .and_then(|authenticator| (!token.is_empty()).then(|| authenticator.authenticate(token)).flatten());
        response_body["apiVersion"] = serde_json::json!("authentication.k8s.io/v1");
        response_body["kind"] = serde_json::json!("TokenReview");
        response_body["status"] = match authenticated {
            Some(authenticated) => serde_json::json!({
                "authenticated": true,
                "user": {
                    "username": authenticated.identity.name,
                    "uid": authenticated.service_account_uid,
                    "groups": authenticated.identity.groups,
                }
            }),
            None => serde_json::json!({"authenticated": false}),
        };
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // Group H: ServiceAccount TokenRequest backs projected pod tokens. The
    // caller must be authorized for the serviceaccounts/token subresource;
    // the ServiceAccount and, when supplied, bound Pod are read from storage
    // before the stateless signer is allowed to mint a token.
    if info.is_resource_request
        && info.api_group.is_empty()
        && info.resource == "serviceaccounts"
        && info.subresource == "token"
        && info.verb == "create"
        && !info.namespace.is_empty()
        && !info.name.is_empty()
    {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let Some(authenticator) = service_account_authenticator.as_deref() else {
            return Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(&path_str, "ServiceAccount token signing is not configured")));
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the TokenRequest body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let request = match crate::authn::service_account::parse_token_request(&body_value) {
            Ok(request) => request,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e))),
        };
        let service_account = match rest::get(&mut client, None, "", "v1", "serviceaccounts", Some(&info.namespace), &info.name).await {
            Ok(rest::GetOutcome::Found(service_account)) => service_account,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "TokenRequest ServiceAccount lookup failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let service_account_uid = service_account
            .pointer("/metadata/uid")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if let Some((pod_name, pod_uid)) = &request.bound_pod {
            match rest::get(&mut client, None, "", "v1", "pods", Some(&info.namespace), pod_name).await {
                Ok(rest::GetOutcome::Found(pod)) if pod.pointer("/metadata/uid").and_then(serde_json::Value::as_str) == Some(pod_uid) => {}
                Ok(rest::GetOutcome::Found(_)) => {
                    return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "bound Pod UID does not match the current Pod")));
                }
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                    return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                }
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "TokenRequest bound Pod lookup failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }
        }
        let issued = match authenticator.issue_token(&info.namespace, &info.name, service_account_uid, &request) {
            Ok(issued) => issued,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let mut response_body = body_value;
        response_body["apiVersion"] = serde_json::json!("authentication.k8s.io/v1");
        response_body["kind"] = serde_json::json!("TokenRequest");
        response_body["status"] = serde_json::json!({
            "token": issued.token,
            "expirationTimestamp": issued.expiration_timestamp,
        });
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // Group L: aggregated APIs (`APIService`) — a genuine live reverse
    // proxy to a real aggregated backend now, Phase 4's remaining wiring.
    // Checked before the generic verb dispatch (every other special-cased
    // route in this function is too), and before `pods/log` right below
    // since an aggregated group could in principle define its own `pods`
    // resource — the check itself costs nothing extra for the vastly more
    // common non-aggregated request (`aggregator::route::resolve` is a
    // bounded `LIST` of `APIService`s only, not per-item I/O, and a
    // request with an empty `api_group` — the core group — short-circuits
    // inside it immediately). See `aggregator::route`/`::client_tls`/
    // `::availability`/`::proxy_target`'s own doc comments for the full
    // design; `aggregate_proxy` below is the dispatch glue, same split
    // `pods/log`'s own branch already established. **Discovery merge
    // (Phase 3) is still not done** — an aggregated group's own
    // `/apis/{group}/{version}` discovery document isn't proxied yet, a
    // real, separate, named gap (`aggregator::mod`'s own doc comment);
    // only resource-shaped requests under an already-known `(group,
    // version)` reach this branch at all, matching real upstream's own
    // "resource requests only" scope for its aggregation proxy handler.
    if info.is_resource_request && !info.api_group.is_empty() {
        if let Some(mut client) = storage.clone() {
            match aggregator::route::resolve(&mut client, &info.api_group, &info.api_version).await {
                Ok(Some(api_service)) => return Ok(aggregate_proxy(req, &method, &api_service, client, &path_str, &query).await),
                Ok(None) => {}
                Err(e) => warn!(path = %path_str, error = ?e, "aggregation: looking up a matching APIService failed"),
            }
        }
    }
    // Group N: pod connection subresources are HTTP upgrades. Resolve the
    // pod and its node here, then let the streaming proxy carry the upgrade
    // through to nodelet. This must run before the generic REST branches:
    // `POST .../pods/{name}/exec` is otherwise indistinguishable from an
    // ordinary create-shaped request to the path parser.
    if info.is_resource_request
        && info.api_group.is_empty()
        && info.resource == "pods"
        && !info.name.is_empty()
        && matches!(info.subresource.as_str(), "exec" | "attach" | "portforward")
        && matches!(method.as_str(), "GET" | "POST")
    {
        let Some(mut client) = storage.clone() else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };

        let pod = match rest::get(&mut client, None, "", "v1", "pods", namespace, &info.name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "proxy: fetching the pod for a streaming subresource failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let node_name = pod.pointer("/spec/nodeName").and_then(serde_json::Value::as_str).unwrap_or("");
        if node_name.is_empty() {
            return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "pod has not yet been scheduled to a node")));
        }
        let node = match rest::get(&mut client, None, "", "v1", "nodes", None, node_name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
            Err(error) => {
                warn!(path = %path_str, node = %node_name, error = ?error, "proxy: fetching the pod node for a streaming subresource failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };

        let pairs = path::parse_query(&query);
        let target = match proxy::pod_stream::target(&pod, &node, &info.subresource, &pairs) {
            Ok(target) => target,
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::NoDefaultContainer { pod_name, candidates })) => {
                let detail = if candidates.is_empty() {
                    format!("a container name must be specified for pod {pod_name}")
                } else {
                    format!("a container name must be specified for pod {pod_name}, choose one of: {candidates:?}")
                };
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &detail)));
            }
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::UnknownContainer { pod_name, container })) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &format!("container {container} is not valid for pod {pod_name}"))));
            }
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::PodNotScheduled)) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "pod has not yet been scheduled to a node")));
            }
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::NoNodeAddress)) => {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
            Err(proxy::pod_stream::Error::MissingPort) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "at least one port is required for port-forward")));
            }
            Err(proxy::pod_stream::Error::InvalidPort(port)) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &format!("invalid port {port}"))));
            }
        };

        return match proxy::http_client::upgrade(req, &target, kubelet_tls).await {
            Ok(response) => Ok(response),
            Err(error) => {
                warn!(path = %path_str, node = %node_name, error = ?error, "proxy: streaming upgrade to nodelet failed");
                Ok(json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(&path_str, &error.to_string())))
            }
        };
    }
    // Group N: `pods/log` — a genuine live proxy to nodelet's own
    // `/containerLogs` endpoint (`crates/nodelet/src/server/logs.rs`),
    // not a stub. See `proxy::pod_log`/`proxy::client_tls`/
    // `proxy::http_client`'s own doc comments for the full design; this
    // branch is just the dispatch glue: fetch the pod, fetch its node,
    // resolve the target (`proxy::pod_log::log_location`), dial nodelet
    // for real, relay its response — status, headers, streaming body —
    // back unmodified. Checked before the generic `is_get` handling below
    // (which requires an empty `subresource`), same "specific virtual/
    // special-cased routes before the generic verb block" ordering every
    // other early-return branch above already uses.
    if info.is_resource_request && info.api_group.is_empty() && info.resource == "pods" && info.subresource == "log" && !info.name.is_empty() && (method == "GET" || method == "HEAD") {
        let Some(mut client) = storage.clone() else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };

        let pod = match rest::get(&mut client, None, "", "v1", "pods", namespace, &info.name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "proxy: fetching the pod for pods/log failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let node_name = pod.get("spec").and_then(|s| s.get("nodeName")).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
        if node_name.is_empty() {
            return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "pod has not yet been scheduled to a node")));
        }
        // Nodes are cluster-scoped -- `namespace: None`, matching every
        // other cluster-scoped `rest::get` call in this module.
        let node = match rest::get(&mut client, None, "", "v1", "nodes", None, &node_name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                warn!(path = %path_str, node = %node_name, "proxy: pod's own node not found for pods/log");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "proxy: fetching the pod's node for pods/log failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };

        let query_pairs = path::parse_query(&query);
        let container = query_pairs.iter().find(|(k, _)| k == "container").map(|(_, v)| v.clone()).unwrap_or_default();
        let target = match proxy::pod_log::log_location(&pod, &node, &container, &query_pairs) {
            Ok(t) => t,
            Err(proxy::pod_log::Error::NoDefaultContainer { pod_name, candidates }) => {
                let detail = if candidates.is_empty() {
                    format!("a container name must be specified for pod {pod_name}")
                } else {
                    format!("a container name must be specified for pod {pod_name}, choose one of: {candidates:?}")
                };
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &detail)));
            }
            Err(proxy::pod_log::Error::UnknownContainer { pod_name, container }) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &format!("container {container} is not valid for pod {pod_name}"))));
            }
            Err(proxy::pod_log::Error::PodNotScheduled) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "pod has not yet been scheduled to a node")));
            }
            Err(proxy::pod_log::Error::NoNodeAddress) => {
                warn!(path = %path_str, node = %node_name, "proxy: node has no address of any preferred type for pods/log");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };

        return match proxy::http_client::fetch(&target, kubelet_tls).await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                warn!(path = %path_str, node = %node_name, error = ?e, "proxy: dialing nodelet for pods/log failed");
                Ok(json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(&path_str, &e.to_string())))
            }
        };
    }

    // WATCH is dispatched below the CRUD block, so retain this negotiated
    // representation before the request body can be consumed by a mutating
    // request.
    let wants_partial_metadata = req
        .headers()
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .and_then(negotiation::negotiate)
        .is_some_and(|accepted| accepted.wants_partial_object_metadata());
    let has_body = is_create || is_update;
