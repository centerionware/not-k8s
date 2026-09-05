include!("handle/subresources.rs");
include!("handle/pod_resize.rs");
include!("handle/patch_apply_admission.rs");
include!("handle/patch_apply.rs");
include!("handle/patch_standard.rs");
include!("handle/patch.rs");
include!("handle/status.rs");
include!("handle/delete_collection.rs");
include!("handle/reviews.rs");
include!("handle/tokens.rs");
include!("handle/scale.rs");
include!("handle/aggregate.rs");
include!("handle/crud/early_admission.rs");
include!("handle/crud/defaults.rs");
include!("handle/crud/late_admission.rs");
include!("handle/crud/persist.rs");
include!("handle/crud.rs");
include!("handle/watch.rs");
async fn handle(
    req: Request<Incoming>,
    mut storage: Option<StorageClient>,
    cache_registry: crate::cacher::CacheRegistry,
    pure_admission: Arc<crate::admission::chain::MutatingRegistry>,
    pod_node_selector_config: Option<Arc<crate::admission::pod_node_selector::PluginConfig>>,
    identity: Option<crate::authn::x509::Identity>,
    service_account_authenticator: Option<
        Arc<crate::authn::service_account::ReloadableAuthenticator>,
    >,
    enforce_rbac: bool,
    authorization_webhook_allowed: bool,
    aggregation_proxy_identity: Option<Arc<crate::aggregator::client_tls::ClientIdentity>>,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Result<Response<BoxedBody>, Infallible> {
    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let request_field_manager = field_manager_query(&query)
        .or_else(|| {
            req.headers()
                .get("user-agent")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty());
    let admission_metadata = req.extensions().get::<SharedAdmissionMetadata>().cloned();

    let info = path::parse(&method, &path_str, &query);

    // Keep authorization ahead of every handler, including health, metrics,
    // and discovery endpoints. These are non-resource requests, but
    // upstream RBAC evaluates them through `nonResourceURLs` just like a
    // resource request is evaluated through `resources`.
    if should_run_local_authorization(&info, enforce_rbac, authorization_webhook_allowed) {
        let Some(client) = storage.as_mut() else {
            return Ok(json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &internal_error_status(&path_str),
            ));
        };
        let allowed = match authz::request_allowed(client, identity.as_ref(), &info, Some(&cache_registry)).await {
            Ok(allowed) => allowed,
            Err(error) => {
                warn!(path = %path_str, error = %error, "node/RBAC authorization failed");
                return Ok(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &internal_error_status(&path_str),
                ));
            }
        };
        if !allowed {
            let user_name = identity
                .as_ref()
                .map(|id| id.name.as_str())
                .unwrap_or(ANONYMOUS_USERNAME);
            return Ok(json_response(
                StatusCode::FORBIDDEN,
                &forbidden_status(&path_str, user_name),
            ));
        }
    }

    if let Some(check_name) = path_str
        .strip_prefix('/')
        .filter(|p| matches!(*p, "healthz" | "readyz" | "livez"))
    {
        let params = path::parse_query(&query);
        let verbose = params.iter().any(|(key, _)| key == "verbose");
        let excluded = params
            .into_iter()
            .filter(|(key, _)| key == "exclude")
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        let storage_healthy = if check_name == "readyz" {
            match storage.as_mut() {
                Some(client) => {
                    tokio::time::timeout(std::time::Duration::from_secs(1), client.is_healthy())
                        .await
                        .unwrap_or(false)
                }
                None => false,
            }
        } else {
            storage.is_some()
        };
        let (checks, unknown_excluded) =
            healthz::run_checks(check_name, storage_healthy, &excluded);
        let (status, body) = healthz::render(check_name, &checks, &unknown_excluded, verbose);
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return Ok(Response::builder()
            .status(code)
            .header("Content-Type", "text/plain; charset=utf-8")
            .header("X-Content-Type-Options", "nosniff")
            .body(body_from_bytes(body.into_bytes()))
            .unwrap());
    }

    if path_str == "/metrics" {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
            .body(body_from_bytes(metrics::render().into_bytes()))
            .unwrap());
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
        let (crds, aggregated) = if parts.first().map(String::as_str) == Some("apis")
            && parts.len() <= 3
        {
            match storage.clone() {
                Some(mut client) => {
                    let crds = match rest::list_all_crds(&mut client).await {
                        Ok(crds) => {
                            crate::apiextensions::registry::discoverable_resources(crds.iter())
                        }
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
                    let aggregated = match aggregator::route::discoverable_group_versions(
                        &mut client,
                        Some(&cache_registry),
                    )
                    .await
                    {
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
            DiscoveryRoute::Found(doc) => {
                return Ok(json_response_with_content_type(
                    StatusCode::OK,
                    &doc,
                    discovery_content_type(&parts, accept_header),
                ));
            }
            DiscoveryRoute::FoundRaw(bytes) => {
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .body(body_from_bytes(bytes.to_vec()))
                    .unwrap());
            }
            DiscoveryRoute::FoundOpenApiProtobuf(bytes) => {
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", openapi::V2_PROTOBUF_CONTENT_TYPE)
                    .header("Vary", "Accept")
                    .body(body_from_bytes(bytes.to_vec()))
                    .unwrap());
            }
            DiscoveryRoute::NotAcceptable => {
                return Ok(json_response(StatusCode::NOT_ACCEPTABLE, &serde_json::json!({
                    "apiVersion": "v1", "kind": "Status", "status": "Failure",
                    "reason": "NotAcceptable", "code": 406,
                    "message": "OpenAPI v2 supports application/json and the gnostic v2 protobuf media type"
                })));
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
                if let Some((group, version)) =
                    aggregated_discovery_group_version(&parts, &aggregated)
                {
                    if let Some(mut client) = storage.clone() {
                        if let Ok(Some(api_service)) =
                            aggregator::route::resolve(&mut client, group, version, Some(&cache_registry)).await
                        {
                            return Ok(aggregate_proxy(
                                req,
                                &method,
                                &api_service,
                                client,
                                &path_str,
                                &query,
                                identity.as_ref(),
                                aggregation_proxy_identity.as_deref(),
                            )
                            .await);
                        }
                    }
                }
                return Ok(json_response(
                    StatusCode::NOT_FOUND,
                    &not_found_status(&path_str),
                ));
            }
            DiscoveryRoute::NotApplicable => {}
        }
    }

    // Resource requests cannot be classified as found or missing without a
    // nodestore connection. Report the unavailable backend instead of
    // allowing the generic not-found path to turn an outage into a false
    // successful server response.
    if info.is_resource_request && storage.is_none() {
        return Ok(json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &service_unavailable_status(&path_str, "storage backend unavailable"),
        ));
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
        && matches!(
            method.as_str(),
            "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
        )
    {
        return Ok(proxy_resource(
            req,
            storage,
            &info,
            &method,
            &path_str,
            &query,
            &identity,
            enforce_rbac,
            kubelet_tls,
        )
        .await);
    }

    // Group E's real resource verbs so far: single-object GET (`get`, not
    // `list`/`watch` — `path::parse` already tells those apart by the
    // verb), LIST (`list`), CREATE (`create`, no name — a POST
    // to the collection URL), single-object DELETE (`delete`, name
    // required — no name means `deletecollection`, now real too — see its
    // own dedicated branch below), and UPDATE (`update`, name
    // required — a PUT). The scheduler's core `pods/binding` and Pod
    // `pods/ephemeralcontainers` subresources are handled separately below;
    // the remaining subresources still fall through (see `rest`'s own doc
    // comment). Everything else returns a real Kubernetes error below. `storage` is only
    // ever consumed once (moved into `client` here), which is why all
    // five verbs share this one `if let` rather than each checking it
    // separately.
    let is_get = info.is_resource_request
        && info.verb == "get"
        && !info.name.is_empty()
        && info.subresource.is_empty();
    let is_list = info.is_resource_request
        && info.verb == "list"
        && info.subresource.is_empty();
    let is_create = info.is_resource_request
        && info.verb == "create"
        && info.name.is_empty()
        && info.subresource.is_empty();
    let is_delete = info.is_resource_request
        && info.verb == "delete"
        && !info.name.is_empty()
        && info.subresource.is_empty();
    let is_update = info.is_resource_request
        && info.verb == "update"
        && !info.name.is_empty()
        && info.subresource.is_empty();
    let is_certificate_status_subresource = info.is_resource_request
        && info.api_group == "certificates.k8s.io"
        && info.resource == "certificatesigningrequests"
        && matches!(info.subresource.as_str(), "approval" | "status");
    // `watch` (no name — `path::parse` already tells a namefull `watch`
    // apart, though real upstream's own single-resource watch form isn't
    // handled specially here either way today) is deliberately handled in
    // its own branch below, not folded into the five-verb block above:
    // unlike those five, it needs no request body, no `storage`/`client`
    // (it's served purely from an already-registered `cacher::CacheRegistry`
    // cache — see that branch's own doc comment), and produces a
    // streaming response rather than one JSON document.
    let is_watch = info.is_resource_request && info.verb == "watch" && info.subresource.is_empty();

    // These values are shared by the CRUD and watch dispatch paths. Compute
    // them before any branch can consume the request body.
    let wants_partial_metadata = req
        .headers()
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .and_then(negotiation::negotiate)
        .is_some_and(|accepted| accepted.wants_partial_object_metadata());
    let has_body = is_create || is_update;

    handle_pod_resize!(req, storage, path_str, query, info, request_field_manager);
    handle_subresources!(
        req,
        storage,
        cache_registry,
        pure_admission,
        pod_node_selector_config,
        identity,
        service_account_authenticator,
        enforce_rbac,
        authorization_webhook_allowed,
        aggregation_proxy_identity,
        kubelet_tls,
        method,
        path_str,
        query,
        info,
        request_field_manager,
        admission_metadata,
        is_get,
        is_list,
        is_create,
        is_delete,
        is_update,
        is_watch,
        is_certificate_status_subresource,
        wants_partial_metadata,
        has_body
    );
    handle_patch!(
        req,
        storage,
        cache_registry,
        pure_admission,
        pod_node_selector_config,
        identity,
        service_account_authenticator,
        enforce_rbac,
        authorization_webhook_allowed,
        aggregation_proxy_identity,
        kubelet_tls,
        method,
        path_str,
        query,
        info,
        request_field_manager,
        admission_metadata,
        is_get,
        is_list,
        is_create,
        is_delete,
        is_update,
        is_watch,
        is_certificate_status_subresource,
        wants_partial_metadata,
        has_body
    );
    handle_status!(
        req,
        storage,
        cache_registry,
        pure_admission,
        pod_node_selector_config,
        identity,
        service_account_authenticator,
        enforce_rbac,
        authorization_webhook_allowed,
        aggregation_proxy_identity,
        kubelet_tls,
        method,
        path_str,
        query,
        info,
        request_field_manager,
        admission_metadata,
        is_get,
        is_list,
        is_create,
        is_delete,
        is_update,
        is_watch,
        is_certificate_status_subresource,
        wants_partial_metadata,
        has_body
    );
    handle_delete_collection!(
        req,
        storage,
        cache_registry,
        pure_admission,
        pod_node_selector_config,
        identity,
        service_account_authenticator,
        enforce_rbac,
        authorization_webhook_allowed,
        aggregation_proxy_identity,
        kubelet_tls,
        method,
        path_str,
        query,
        info,
        request_field_manager,
        admission_metadata,
        is_get,
        is_list,
        is_create,
        is_delete,
        is_update,
        is_watch,
        is_certificate_status_subresource,
        wants_partial_metadata,
        has_body
    );
    handle_reviews!(
        req,
        storage,
        cache_registry,
        pure_admission,
        pod_node_selector_config,
        identity,
        service_account_authenticator,
        enforce_rbac,
        authorization_webhook_allowed,
        aggregation_proxy_identity,
        kubelet_tls,
        method,
        path_str,
        query,
        info,
        request_field_manager,
        admission_metadata,
        is_get,
        is_list,
        is_create,
        is_delete,
        is_update,
        is_watch,
        is_certificate_status_subresource,
        wants_partial_metadata,
        has_body
    );
    handle_tokens!(
        req,
        storage,
        cache_registry,
        pure_admission,
        pod_node_selector_config,
        identity,
        service_account_authenticator,
        enforce_rbac,
        authorization_webhook_allowed,
        aggregation_proxy_identity,
        kubelet_tls,
        method,
        path_str,
        query,
        info,
        request_field_manager,
        admission_metadata,
        is_get,
        is_list,
        is_create,
        is_delete,
        is_update,
        is_watch,
        is_certificate_status_subresource,
        wants_partial_metadata,
        has_body
    );
    handle_scale!(
        req,
        storage,
        cache_registry,
        pure_admission,
        pod_node_selector_config,
        identity,
        service_account_authenticator,
        enforce_rbac,
        authorization_webhook_allowed,
        aggregation_proxy_identity,
        kubelet_tls,
        method,
        path_str,
        query,
        info,
        request_field_manager,
        admission_metadata,
        is_get,
        is_list,
        is_create,
        is_delete,
        is_update,
        is_watch,
        is_certificate_status_subresource,
        wants_partial_metadata,
        has_body
    );
    handle_aggregate!(
        req,
        storage,
        cache_registry,
        pure_admission,
        pod_node_selector_config,
        identity,
        service_account_authenticator,
        enforce_rbac,
        authorization_webhook_allowed,
        aggregation_proxy_identity,
        kubelet_tls,
        method,
        path_str,
        query,
        info,
        request_field_manager,
        admission_metadata,
        is_get,
        is_list,
        is_create,
        is_delete,
        is_update,
        is_watch,
        is_certificate_status_subresource,
        wants_partial_metadata,
        has_body
    );
    handle_crud!(
        req,
        storage,
        cache_registry,
        pure_admission,
        pod_node_selector_config,
        identity,
        service_account_authenticator,
        enforce_rbac,
        authorization_webhook_allowed,
        aggregation_proxy_identity,
        kubelet_tls,
        method,
        path_str,
        query,
        info,
        request_field_manager,
        admission_metadata,
        is_get,
        is_list,
        is_create,
        is_delete,
        is_update,
        is_watch,
        is_certificate_status_subresource,
        wants_partial_metadata,
        has_body
    );
    return handle_watch!(
        req,
        storage,
        cache_registry,
        pure_admission,
        pod_node_selector_config,
        identity,
        service_account_authenticator,
        enforce_rbac,
        authorization_webhook_allowed,
        aggregation_proxy_identity,
        kubelet_tls,
        method,
        path_str,
        query,
        info,
        request_field_manager,
        admission_metadata,
        is_get,
        is_list,
        is_create,
        is_delete,
        is_update,
        is_watch,
        is_certificate_status_subresource,
        wants_partial_metadata,
        has_body
    );
}
