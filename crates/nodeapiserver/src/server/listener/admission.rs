/// Group M: wraps every request with a real `audit::event::build_event`
/// call, logged rather than delegated back into `handle` itself. The
/// wrapper keeps the audit context at the request boundary and explicitly
/// records responses that finish before `handle` runs, while the normal
/// response path is audited after `handle` returns. The sink is this crate's
/// own `tracing` output (`target: "nodeapiserver::audit"`, one JSON line per
/// request) and, when configured, an append-only file selected by
/// `NODEAPISERVER_AUDIT_LOG_PATH` or a bounded asynchronous webhook selected
/// by `NODEAPISERVER_AUDIT_WEBHOOK_URL`. See
/// `audit::event`'s own doc comment for exactly which real `Event`
/// fields are populated and which stages/levels this uses.
async fn handle_with_audit(
    req: Request<Incoming>,
    storage: Option<StorageClient>,
    cache_registry: crate::cacher::CacheRegistry,
    pure_admission: Arc<crate::admission::chain::MutatingRegistry>,
    pod_node_selector_config: Option<Arc<crate::admission::pod_node_selector::PluginConfig>>,
    identity: Option<crate::authn::x509::Identity>,
    bootstrap_token_authenticator: Option<Arc<crate::authn::bootstrap_token::ReloadableAuthenticator>>,
    service_account_authenticator: Option<Arc<crate::authn::service_account::ReloadableAuthenticator>>,
    oidc_authenticator: Option<Arc<crate::authn::oidc::Authenticator>>,
    authorization_webhook: Option<Arc<crate::authz::webhook::WebhookAuthorizer>>,
    aggregation_proxy_identity: Option<Arc<crate::aggregator::client_tls::ClientIdentity>>,
    concurrency_limiter: Arc<crate::flowcontrol::limiter::ConcurrencyLimiter>,
    audit_sink: Option<Arc<crate::audit::sink::AuditSink>>,
    audit_policy: Option<Arc<crate::audit::policy::AuditPolicy>>,
    anonymous_auth: bool,
    enforce_rbac: bool,
    max_request_body_bytes: usize,
    peer: SocketAddr,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Result<Response<BoxedBody>, Infallible> {
    let admission_metadata = Arc::new(Mutex::new(AdmissionMetadata::default()));
    let mut req = req;
    req.extensions_mut().insert(admission_metadata.clone());
    req.extensions_mut().insert(RequestBodyLimit(max_request_body_bytes));
    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let user_agent = req.headers().get("user-agent").and_then(|v| v.to_str().ok()).map(str::to_string);
    let request_info = path::parse(&method, &path_str, &query);
    let audit_id = uuid::Uuid::new_v4().to_string();
    let identity = match authenticate_request(
        &req,
        identity,
        bootstrap_token_authenticator.as_deref(),
        service_account_authenticator.as_deref(),
        oidc_authenticator.as_deref(),
        anonymous_auth,
    )
    .await
    {
        Ok(identity) => identity,
        Err(detail) => {
            let response = json_response(
                StatusCode::UNAUTHORIZED,
                &unauthorized_status(&path_str, detail),
            );
            log_audit_rejected_request(
                &audit_id,
                &request_info,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                None,
                &peer,
                response.status().as_u16(),
                audit_sink.as_deref(),
                audit_policy.as_deref(),
            );
            return Ok(response);
        }
    };
    let audit_identity = identity.clone();
    let audit_user = audit_identity.as_ref().map(|identity| identity.name.as_str()).unwrap_or(ANONYMOUS_USERNAME);
    let audit_groups = audit_identity
        .as_ref()
        .map(|identity| identity.groups.clone())
        .unwrap_or_else(|| vec![UNAUTHENTICATED_GROUP.to_string()]);
    let long_running = is_long_running_request(&request_info, &query);
    let audit_level = audit_policy
        .as_ref()
        .map(|policy| policy.decide(&request_info, audit_user, &audit_groups).level)
        .unwrap_or(crate::audit::policy::Level::Metadata);
    let capture_request_body = !long_running
        && matches!(
            audit_level,
            crate::audit::policy::Level::Request | crate::audit::policy::Level::RequestResponse
        );
    let request_body_capture = capture_request_body.then(|| {
        let capture = Arc::new(Mutex::new(None));
        req.extensions_mut().insert(AuditRequestBodyCapture(capture.clone()));
        capture
    });
    let request_content_type = req
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let audit_request_received = audit_policy.as_ref().is_some_and(|policy| {
        policy.should_emit_stage(
            &request_info,
            audit_user,
            &audit_groups,
            crate::audit::event::STAGE_REQUEST_RECEIVED,
        )
    });
    let audit_response_started = long_running
        && audit_policy.as_ref().map_or(true, |policy| {
            policy.should_emit_stage(
                &request_info,
                audit_user,
                &audit_groups,
                crate::audit::event::STAGE_RESPONSE_STARTED,
            )
        });
    let audit_response_complete = !long_running
        && audit_policy.as_ref().map_or(true, |policy| {
            policy.should_emit_stage(
                &request_info,
                audit_user,
                &audit_groups,
                crate::audit::event::STAGE_RESPONSE_COMPLETE,
            )
        });
    if audit_request_received && !capture_request_body {
        log_audit_event(
            &audit_id,
            crate::audit::event::STAGE_REQUEST_RECEIVED,
            &method,
            &path_str,
            &query,
            user_agent.as_deref(),
            audit_identity.as_ref(),
            &peer,
            0,
            audit_sink.as_deref(),
            &BTreeMap::new(),
        );
    }
    let mut authorization_webhook_allowed = false;
    if let Some(authorizer) = authorization_webhook {
        match authorizer
            .authorize_with_details(&request_info, identity.as_ref())
            .await
        {
            Ok(details) => {
                if let Some(error) = details.evaluation_error.as_deref() {
                    warn!(path = %path_str, evaluation_error = %error, "authorization webhook returned an evaluation error");
                }
                match details.decision {
                    crate::authz::webhook::Decision::Allow => {
                        authorization_webhook_allowed = true;
                    }
                    crate::authz::webhook::Decision::NoOpinion => {}
                    crate::authz::webhook::Decision::Deny => {
                        let user_name = identity
                            .as_ref()
                            .map(|identity| identity.name.as_str())
                            .unwrap_or(ANONYMOUS_USERNAME);
                        let response = json_response(
                            StatusCode::FORBIDDEN,
                            &forbidden_status_with_reason(
                                &path_str,
                                user_name,
                                &details.reason,
                            ),
                        );
                        log_audit_rejected_request(
                            &audit_id,
                            &request_info,
                            &method,
                            &path_str,
                            &query,
                            user_agent.as_deref(),
                            identity.as_ref(),
                            &peer,
                            response.status().as_u16(),
                            audit_sink.as_deref(),
                            audit_policy.as_deref(),
                        );
                        return Ok(response);
                    }
                }
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "authorization webhook failed");
                let response = json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &service_unavailable_status(&path_str, "authorization webhook unavailable"),
                );
                log_audit_rejected_request(
                    &audit_id,
                    &request_info,
                    &method,
                    &path_str,
                    &query,
                    user_agent.as_deref(),
                    identity.as_ref(),
                    &peer,
                    response.status().as_u16(),
                    audit_sink.as_deref(),
                    audit_policy.as_deref(),
                );
                return Ok(response);
            }
        }
    }
    let selected_priority = if let Some(mut client) = storage.clone() {
        let (user_name, user_groups): (&str, Vec<String>) = match identity.as_ref() {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let digest = flowcontrol::flow_schema::RequestDigest {
            user_name,
            user_groups: &user_groups,
            verb: &request_info.verb,
            is_resource_request: request_info.is_resource_request,
            api_group: &request_info.api_group,
            resource: &request_info.resource,
            subresource: &request_info.subresource,
            namespace: &request_info.namespace,
            path: &request_info.path,
        };
        flowcontrol::resolve::select_for_request(&mut client, &digest).await
    } else {
        None
    };
    let selected_priority_config = selected_priority.as_ref().map(|selected| &selected.priority_level);
    let configured_priorities = selected_priority
        .as_ref()
        .map(|selected| selected.priority_levels.as_slice())
        .unwrap_or(&[]);
    let flow_distinguisher = selected_priority.as_ref().map(|selected| selected.flow_distinguisher.as_str()).unwrap_or("");
    let _permit = match concurrency_limiter
        .acquire_with_priorities(&request_info, &query, selected_priority_config, configured_priorities, flow_distinguisher)
        .await
    {
        Ok(permit) => permit,
        Err(crate::flowcontrol::limiter::Error::QueueFull) => {
            let response = json_response(
                StatusCode::TOO_MANY_REQUESTS,
                &too_many_requests_status(&path_str),
            );
            log_audit_rejected_request(
                &audit_id,
                &request_info,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                identity.as_ref(),
                &peer,
                response.status().as_u16(),
                audit_sink.as_deref(),
                audit_policy.as_deref(),
            );
            return Ok(response);
        }
        Err(crate::flowcontrol::limiter::Error::Closed) => {
            let response = json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &service_unavailable_status(
                    &path_str,
                    "API request concurrency limiter is unavailable",
                ),
            );
            log_audit_rejected_request(
                &audit_id,
                &request_info,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                identity.as_ref(),
                &peer,
                response.status().as_u16(),
                audit_sink.as_deref(),
                audit_policy.as_deref(),
            );
            return Ok(response);
        }
    };
    let _inflight = _permit
        .as_ref()
        .map(|_| metrics::begin_inflight(is_mutating_request(&request_info)));

    // Group M: `apiserver_request_duration_seconds`'s own start time —
    // measured around the exact same `handle()` call the audit event and
    // `apiserver_request_total` are both already keyed off of. For
    // `watch` specifically this measures time-to-first-byte (when
    // `handle()` returns the still-streaming response), not the full
    // stream lifetime.
    let start = std::time::Instant::now();
    let mut response_object = None;
    let mut response = match handle(req, storage, cache_registry, pure_admission, pod_node_selector_config, identity, service_account_authenticator, enforce_rbac, authorization_webhook_allowed, aggregation_proxy_identity, kubelet_tls).await {
        Ok(response) => {
            if audit_level == crate::audit::policy::Level::RequestResponse && !long_running {
                let (response, object) = capture_response_object(response, max_request_body_bytes).await;
                response_object = object;
                Ok(response)
            } else {
                Ok(response)
            }
        }
        Err(error) => match error {},
    };
    let elapsed = start.elapsed().as_secs_f64();

    if let Ok(resp) = &mut response {
        let metadata = admission_metadata.lock().map(|metadata| metadata.clone()).unwrap_or_default();
        apply_admission_warnings(resp, &metadata.warnings);
        let audit_annotations = audit_annotations(&metadata);
        let status = resp.status().as_u16();
        let request_object = request_body_capture
            .as_ref()
            .and_then(|capture| capture.lock().ok().and_then(|captured| captured.clone()))
            .as_deref()
            .and_then(|bytes| decode_audit_object(bytes, request_content_type.as_deref()));
        if audit_request_received && capture_request_body {
            log_audit_event_with_objects(
                &audit_id,
                crate::audit::event::STAGE_REQUEST_RECEIVED,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                audit_identity.as_ref(),
                &peer,
                0,
                audit_sink.as_deref(),
                &BTreeMap::new(),
                audit_level.as_str(),
                request_object.as_ref(),
                None,
            );
        }
        if audit_response_started {
            log_audit_event_with_objects(
                &audit_id,
                crate::audit::event::STAGE_RESPONSE_STARTED,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                audit_identity.as_ref(),
                &peer,
                status,
                audit_sink.as_deref(),
                &audit_annotations,
                audit_level.as_str(),
                request_object.as_ref(),
                None,
            );
        }
        if audit_response_complete {
            log_audit_event_with_objects(
                &audit_id,
                crate::audit::event::STAGE_RESPONSE_COMPLETE,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                audit_identity.as_ref(),
                &peer,
                status,
                audit_sink.as_deref(),
                &audit_annotations,
                audit_level.as_str(),
                request_object.as_ref(),
                response_object.as_ref(),
            );
        }
        // Group M: record the complete upstream-shaped metric label set from
        // the exact same parsed RequestInfo the audit event above builds.
        let info = &request_info;
        let metric_labels = metrics::labels_for_request(info, &query);
        metrics::record_request(&metric_labels, status);
        metrics::record_duration(&metric_labels, elapsed);
        // Group M: `apiserver_response_sizes` — only recorded when the
        // body's own size is known up front (`size_hint().exact()`,
        // `None` for a `watch`'s unbounded stream) — see `server::
        // metrics`'s own doc comment for why that's a real, named,
        // narrower scope than real upstream's own byte-counting
        // instrumentation, not a silent gap.
        {
            use http_body::Body as _;
            if let Some(size) = resp.body().size_hint().exact() {
                metrics::record_response_size(&metric_labels, size);
            }
        }

        // Group M (APF): label the response with the FlowSchema and
        // PriorityLevelConfiguration selected before the request entered
        // the bounded concurrency gate.
        if let Some(selected) = selected_priority {
            if let (Ok(fs), Ok(pl)) = (
                hyper::header::HeaderValue::from_str(&selected.flow_schema_uid),
                hyper::header::HeaderValue::from_str(&selected.priority_level_uid),
            ) {
                resp.headers_mut().insert(flowcontrol::resolve::FLOW_SCHEMA_UID_HEADER, fs);
                resp.headers_mut().insert(flowcontrol::resolve::PRIORITY_LEVEL_UID_HEADER, pl);
            }
        }
    }
    response
}
fn is_mutating_request(info: &path::RequestInfo) -> bool {
    matches!(
        info.verb.as_str(),
        "create" | "update" | "patch" | "delete" | "deletecollection"
    )
}

fn is_long_running_request(info: &path::RequestInfo, query: &str) -> bool {
    if matches!(info.verb.as_str(), "watch" | "proxy")
        || matches!(info.subresource.as_str(), "exec" | "attach" | "portforward")
    {
        return true;
    }
    info.subresource == "log"
        && path::parse_query(query).iter().any(|(key, value)| {
            key == "follow" && !matches!(value.as_str(), "" | "0" | "false")
        })
}

fn log_audit_event(
    audit_id: &str,
    stage: &str,
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    audit_sink: Option<&crate::audit::sink::AuditSink>,
    annotations: &BTreeMap<String, String>,
) {
    log_audit_event_with_objects(
        audit_id,
        stage,
        method,
        path_str,
        query,
        user_agent,
        identity,
        peer,
        status,
        audit_sink,
        annotations,
        crate::audit::event::LEVEL_METADATA,
        None,
        None,
    );
}

fn log_audit_event_with_objects(
    audit_id: &str,
    stage: &str,
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    audit_sink: Option<&crate::audit::sink::AuditSink>,
    annotations: &BTreeMap<String, String>,
    level: &str,
    request_object: Option<&Value>,
    response_object: Option<&Value>,
) {
    let event = build_audit_event_at_stage_with_objects(
        audit_id,
        stage,
        method,
        path_str,
        query,
        user_agent,
        identity,
        peer,
        status,
        annotations,
        level,
        request_object,
        response_object,
    );
    if let Some(sink) = audit_sink {
        if let Err(error) = sink.write(&event) {
            warn!(error = ?error, "nodeapiserver: failed to write audit event");
        }
    }
    tracing::info!(target: "nodeapiserver::audit", "{event}");
}

fn log_audit_rejected_request(
    audit_id: &str,
    info: &path::RequestInfo,
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    audit_sink: Option<&crate::audit::sink::AuditSink>,
    audit_policy: Option<&crate::audit::policy::AuditPolicy>,
) {
    let (user_name, user_groups): (&str, Vec<String>) = match identity {
        Some(identity) => (identity.name.as_str(), identity.groups.clone()),
        None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
    };
    if audit_policy.is_some_and(|policy| {
        policy.should_emit_stage(
            info,
            user_name,
            &user_groups,
            crate::audit::event::STAGE_REQUEST_RECEIVED,
        )
    }) {
        log_audit_event(
            audit_id,
            crate::audit::event::STAGE_REQUEST_RECEIVED,
            method,
            path_str,
            query,
            user_agent,
            identity,
            peer,
            0,
            audit_sink,
            &BTreeMap::new(),
        );
    }
    if audit_policy.map_or(true, |policy| {
        policy.should_emit_stage(
            info,
            user_name,
            &user_groups,
            crate::audit::event::STAGE_RESPONSE_COMPLETE,
        )
    }) {
        log_audit_event(
            audit_id,
            crate::audit::event::STAGE_RESPONSE_COMPLETE,
            method,
            path_str,
            query,
            user_agent,
            identity,
            peer,
            status,
            audit_sink,
            &BTreeMap::new(),
        );
    }
}

/// The pure half of [`log_audit_event`] — everything up to the built
/// `Value`, factored out so it's unit-testable without capturing
/// `tracing`'s own log output.
#[cfg(test)]
fn build_audit_event(
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    annotations: &BTreeMap<String, String>,
) -> serde_json::Value {
    let audit_id = uuid::Uuid::new_v4().to_string();
    build_audit_event_at_stage(
        &audit_id,
        crate::audit::event::STAGE_RESPONSE_COMPLETE,
        method,
        path_str,
        query,
        user_agent,
        identity,
        peer,
        status,
        annotations,
    )
}

#[cfg(test)]
fn build_audit_event_at_stage(
    audit_id: &str,
    stage: &str,
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    annotations: &BTreeMap<String, String>,
) -> serde_json::Value {
    build_audit_event_at_stage_with_objects(
        audit_id,
        stage,
        method,
        path_str,
        query,
        user_agent,
        identity,
        peer,
        status,
        annotations,
        crate::audit::event::LEVEL_METADATA,
        None,
        None,
    )
}

fn build_audit_event_at_stage_with_objects(
    audit_id: &str,
    stage: &str,
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    annotations: &BTreeMap<String, String>,
    level: &str,
    request_object: Option<&Value>,
    response_object: Option<&Value>,
) -> serde_json::Value {
    let info = path::parse(method, path_str, query);
    let anonymous_extra = BTreeMap::new();
    let (user_name, user_uid, user_groups, user_extra): (&str, Option<&str>, Vec<String>, &BTreeMap<String, Vec<String>>) = match identity {
        Some(id) => (id.name.as_str(), id.uid.as_deref(), id.groups.clone(), &id.extra),
        None => (ANONYMOUS_USERNAME, None, vec![UNAUTHENTICATED_GROUP.to_string()], &anonymous_extra),
    };
    let object_ref = info.is_resource_request.then(|| crate::audit::event::ObjectRef { group: &info.api_group, resource: &info.resource, namespace: &info.namespace, name: &info.name, api_version: &info.api_version });
    let request_uri = if query.is_empty() { path_str.to_string() } else { format!("{path_str}?{query}") };
    let timestamp = chrono::Utc::now().to_rfc3339();
    let source_ip = peer.ip().to_string();
    crate::audit::event::build_event_at_stage_with_level(&crate::audit::event::EventInput {
        audit_id,
        request_uri: &request_uri,
        verb: &info.verb,
        user_name,
        user_uid,
        user_groups: user_groups.as_slice(),
        user_extra,
        source_ip: Some(&source_ip),
        user_agent,
        object_ref,
        response_code: status,
        annotations: (!annotations.is_empty()).then_some(annotations),
        request_object,
        response_object,
        timestamp: &timestamp,
    }, level, stage)
}

async fn authenticate_request(
    req: &Request<Incoming>,
    client_cert_identity: Option<crate::authn::x509::Identity>,
    bootstrap_token_authenticator: Option<&crate::authn::bootstrap_token::ReloadableAuthenticator>,
    service_account_authenticator: Option<&crate::authn::service_account::ReloadableAuthenticator>,
    oidc_authenticator: Option<&crate::authn::oidc::Authenticator>,
    anonymous_auth: bool,
) -> std::result::Result<Option<crate::authn::x509::Identity>, &'static str> {
    if client_cert_identity.is_some() {
        return Ok(client_cert_identity);
    }
    let Some(header) = req.headers().get("authorization") else {
        return if anonymous_auth { Ok(None) } else { Err("anonymous authentication is disabled") };
    };
    let value = header.to_str().map_err(|_| "Authorization header is not valid UTF-8")?;
    let Some(token) = value.strip_prefix("Bearer ").filter(|token| !token.is_empty()) else {
        return Err("Authorization must use the Bearer scheme");
    };
    if let Some(authenticator) = bootstrap_token_authenticator {
        if let Some(authenticated) = authenticator.authenticate(token) {
            return Ok(Some(authenticated.identity));
        }
    }
    if let Some(authenticator) = service_account_authenticator {
        if let Some(authenticated) = authenticator.authenticate(token) {
            return Ok(Some(authenticated.identity));
        }
    }
    if let Some(authenticator) = oidc_authenticator {
        if let Some(identity) = authenticator.authenticate(token).await {
            return Ok(Some(identity));
        }
    }
    if bootstrap_token_authenticator.is_none()
        && service_account_authenticator.is_none()
        && oidc_authenticator.is_none()
    {
        return Err("bearer-token authentication is not configured");
    }
    Err("bearer token is invalid or expired")
}

/// Return the namespace segment used by the REST storage key.
///
/// The upstream-compatible path parser keeps the second segment of
/// `/api/v1/namespaces/{name}` in `RequestInfo::namespace`, even though a
/// Namespace object is cluster-scoped. Do not turn that object name into a
/// storage namespace.
fn storage_namespace(info: &path::RequestInfo) -> Option<&str> {
    if info.namespace.is_empty()
        || (info.api_group.is_empty() && info.api_version == "v1" && info.resource == "namespaces")
    {
        None
    } else {
        Some(info.namespace.as_str())
    }
}

/// Run the immutable pure-mutator registry against the candidate produced by
/// any write-shaped REST path. Keeping this call at the candidate boundary
/// prevents ordinary CREATE/UPDATE, PATCH, and Server-Side Apply from
/// accidentally observing different admission behavior.
fn run_pure_admission(
    registry: &crate::admission::chain::MutatingRegistry,
    operation: admission::attributes::Operation,
    info: &path::RequestInfo,
    old_object: Option<&Value>,
    object: &mut Value,
) {
    let mut request = admission::chain::Request {
        operation,
        group: &info.api_group,
        resource: &info.resource,
        subresource: &info.subresource,
        namespace: &info.namespace,
        name: &info.name,
        old_object,
        object,
    };
    registry.run(&mut request);
}
