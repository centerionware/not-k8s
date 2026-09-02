    let admission_metadata = Arc::new(Mutex::new(AdmissionMetadata::default()));
    let mut req = req;
    req.extensions_mut().insert(admission_metadata.clone());
    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let user_agent = req.headers().get("user-agent").and_then(|v| v.to_str().ok()).map(str::to_string);
    let identity = match authenticate_request(&req, identity, bootstrap_token_authenticator.as_deref(), service_account_authenticator.as_deref(), oidc_authenticator.as_deref(), anonymous_auth).await {
        Ok(identity) => identity,
        Err(detail) => return Ok(json_response(StatusCode::UNAUTHORIZED, &unauthorized_status(&path_str, detail))),
    };
    let audit_identity = identity.clone();
    let request_info = path::parse(&method, &path_str, &query);
    let audit_user = audit_identity.as_ref().map(|identity| identity.name.as_str()).unwrap_or(ANONYMOUS_USERNAME);
    let audit_groups = audit_identity
        .as_ref()
        .map(|identity| identity.groups.clone())
        .unwrap_or_else(|| vec![UNAUTHENTICATED_GROUP.to_string()]);
    let audit_response_complete = audit_policy
        .as_ref()
        .map_or(true, |policy| policy.should_emit_response_complete(&request_info, audit_user, &audit_groups));
    let mut authorization_webhook_allowed = false;
    if let Some(authorizer) = authorization_webhook {
        match authorizer.authorize(&request_info, identity.as_ref()).await {
            Ok(crate::authz::webhook::Decision::Allow) => {
                authorization_webhook_allowed = true;
            }
            Ok(crate::authz::webhook::Decision::NoOpinion) => {}
            Ok(crate::authz::webhook::Decision::Deny) => {
                let user_name = identity
                    .as_ref()
                    .map(|identity| identity.name.as_str())
                    .unwrap_or(ANONYMOUS_USERNAME);
                return Ok(json_response(
                    StatusCode::FORBIDDEN,
                    &forbidden_status(&path_str, user_name),
                ));
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "authorization webhook failed");
                return Ok(json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &service_unavailable_status(&path_str, "authorization webhook unavailable"),
                ));
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
            return Ok(json_response(StatusCode::TOO_MANY_REQUESTS, &too_many_requests_status(&path_str)));
        }
        Err(crate::flowcontrol::limiter::Error::Closed) => {
            return Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(&path_str, "API request concurrency limiter is unavailable")));
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
    // stream lifetime — the identical, already-named caveat
    // `log_audit_event`'s own `ResponseComplete`-at-stream-start choice
    // has, not a new gap this metric introduces.
    let start = std::time::Instant::now();
    let mut response = handle(req, storage, cache_registry, identity, service_account_authenticator, enforce_rbac, authorization_webhook_allowed, kubelet_tls).await;
    let elapsed = start.elapsed().as_secs_f64();

    if let Ok(resp) = &mut response {
        let metadata = admission_metadata.lock().map(|metadata| metadata.clone()).unwrap_or_default();
        apply_admission_warnings(resp, &metadata.warnings);
        let audit_annotations = audit_annotations(&metadata);
        let status = resp.status().as_u16();
        if audit_response_complete {
            log_audit_event(&method, &path_str, &query, user_agent.as_deref(), audit_identity.as_ref(), &peer, status, audit_sink.as_deref(), &audit_annotations);
        }
        // Group M: `/metrics`'s own request counter (`server::metrics`) —
        // recorded from the exact same parsed `RequestInfo` the audit
        // event above already builds, so a non-resource request (a
        // discovery route, `/healthz`, ...) is counted under its real
        // verb with an empty `resource` label, matching real upstream's
        // own convention for that case.
        let info = &request_info;
        metrics::record_request(&info.verb, &info.resource, status);
        metrics::record_duration(&info.verb, &info.resource, elapsed);
        // Group M: `apiserver_response_sizes` — only recorded when the
        // body's own size is known up front (`size_hint().exact()`,
        // `None` for a `watch`'s unbounded stream) — see `server::
        // metrics`'s own doc comment for why that's a real, named,
        // narrower scope than real upstream's own byte-counting
        // instrumentation, not a silent gap.
        {
            use http_body::Body as _;
            if let Some(size) = resp.body().size_hint().exact() {
                metrics::record_response_size(&info.verb, &info.resource, size);
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
