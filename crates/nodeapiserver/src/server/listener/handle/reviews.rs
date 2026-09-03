macro_rules! handle_reviews {
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
    // Group I: `SubjectAccessReview`/`SelfSubjectAccessReview`/
    // `LocalSubjectAccessReview` — its own branch, checked before the
    // generic `$is_create` handling below, because it's a virtual
    // resource: real upstream never persists any of the three kinds to
    // $storage (`pkg/registry/authorization/subjectaccessreview`'s own
    // synthetic REST connector), and letting this fall through to the
    // generic `rest::create` path would actually try to write one to
    // nodestore — a real, wrong side effect this early return prevents.
    // Unconditional, not gated by `$enforce_rbac`: answering "would RBAC
    // allow this" is a read on the RBAC engine's own state, not itself
    // an enforcement decision.
    if $info.is_resource_request
        && $info.api_group == "authorization.k8s.io"
        && matches!($info.resource.as_str(), "subjectaccessreviews" | "selfsubjectaccessreviews" | "localsubjectaccessreviews")
        && $info.verb == "create"
        && $info.subresource.is_empty()
    {
        let Some(mut client) = $storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
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
        let (fallback_user, fallback_groups): (&str, Vec<String>) = match &$identity {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let spec = body_value.get("spec").cloned().unwrap_or_default();
        let mut review = match authz::sar::parse_spec(&spec, fallback_user, &fallback_groups) {
            Ok(r) => r,
            Err(msg) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &msg))),
        };
        // `LocalSubjectAccessReview`'s own real semantics: the namespace
        // is the URL's, not whatever (if anything) the submitted
        // `resourceAttributes.namespace` said — the same "the URL is
        // authoritative over the body" rule `rest::update`'s own
        // namespace-mismatch check already establishes elsewhere in this
        // crate, applied here as an override rather than a rejection
        // since a `LocalSubjectAccessReview` isn't required to name a
        // namespace in its body at all.
        if $info.resource == "localsubjectaccessreviews" {
            review.namespace = $info.namespace.clone();
        }
        // Non-resource rules are only ever granted via ClusterRoleBindings
        // in real RBAC too (a namespace-scoped RoleBinding can't grant a
        // non-resource-URL permission) -- resolving with an empty
        // namespace naturally restricts to just those, no separate branch
        // needed.
        let resolve_namespace = if review.is_resource { review.namespace.as_str() } else { "" };
        let resolved = authz::resolve::rules_for(&mut client, &review.user_name, &review.user_groups, resolve_namespace, None).await;
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
    if $info.is_resource_request && $info.api_group == "authorization.k8s.io" && $info.resource == "selfsubjectrulesreviews" && $info.verb == "create" && $info.subresource.is_empty() {
        let Some(mut client) = $storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&$path_str)));
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
        let (user_name, user_groups): (&str, Vec<String>) = match &$identity {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let review_namespace = body_value.pointer("/spec/namespace").and_then(serde_json::Value::as_str).unwrap_or("");
        let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, review_namespace, None).await;
        let mut response_body = body_value;
        response_body["status"] = authz::sar::build_rules_status(&resolved.rules, &resolved.errors);
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // `SelfSubjectReview` (`kubectl auth whoami`) — the simplest of this
    // crate's virtual resources: no $storage, no RBAC, purely reflects
    // whatever $identity `authn::x509` (or the real anonymous fallback)
    // already produced. Same "checked before generic `$is_create`, never
    // persisted" reasoning as every other review kind above.
    if $info.is_resource_request && $info.api_group == "authentication.k8s.io" && $info.resource == "selfsubjectreviews" && $info.verb == "create" && $info.subresource.is_empty() {
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
        let anonymous_extra = BTreeMap::new();
        let (username, uid, groups, extra): (&str, Option<&str>, Vec<String>, &BTreeMap<String, Vec<String>>) = match &$identity {
            Some(id) => (id.name.as_str(), id.uid.as_deref(), id.groups.clone(), &id.extra),
            None => (ANONYMOUS_USERNAME, None, vec![UNAUTHENTICATED_GROUP.to_string()], &anonymous_extra),
        };
        let mut response_body = body_value;
        response_body["status"] = crate::authn::self_review::build_status(username, uid, &groups, extra);
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    }};
}
