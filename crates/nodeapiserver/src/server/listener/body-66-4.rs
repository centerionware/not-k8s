    // Group D/E: real `WATCH`, served purely from an already-registered
    // `cacher::CacheRegistry` cache. A live cache already holds
    // everything the read side of this handler needs (a snapshot to
    // replay from, a live event subscription), and if a resource has no
    // registered cache, this falls through to the RequestInfo echo below
    // exactly like the "no nodestore connection" case above, rather than
    // claiming a real watch this build can't actually serve.
    //
    // Group I: RBAC, gated by `enforce_rbac` same as every other verb —
    // resolved against a fresh `storage.clone()` (cheap — a
    // `tonic::transport::Channel` clone, same as every other real call
    // site), since `watch` doesn't otherwise need `storage`/`client` at
    // all. Unlike a request this build can *choose* to allow when RBAC is
    // off, "enforcement is on but there's no storage connection to
    // resolve rules against" fails closed (`500`), never silently
    // degrading to "allow" — the whole reason `enforce_rbac` exists is to
    // guarantee a denial-capable policy actually ran. Group J admission
    // intentionally does **not** gate `watch` here, matching real
    // upstream's own posture (admission never runs on a read, whatever
    // the verb) — not a gap.
    if is_watch {
        // Group K: an already-registered cache first (unchanged), else —
        // only when the static table doesn't know this resource at all —
        // a live check against the dynamic CRD registry, lazily spawning
        // a cache for it right now on this, its first-ever watch request
        // (`cacher::registry::CacheRegistry::spawn` is callable at any
        // time, not just at boot — see its own doc comment). Only
        // a resource the static table has never heard of falls through to
        // the dynamic check, so this never masks a genuine 404 as "maybe
        // a CRD." Proactive CRD lifecycle reconciliation is started with
        // the listener's built-in CRD cache above; this lazy path remains
        // only as a bounded startup-race fallback for a CRD that is
        // discovered before that reconciler has registered its cache.
        let cache_and_kind: Option<(
            crate::cacher::store::SharedCache,
            String,
            Option<crate::apiextensions::registry::ConversionWebhook>,
        )> = if let Some(cache) = cache_registry.get(&info.api_group, &info.api_version, &info.resource) {
            if let Some(kind) = rest::resolve_kind(&info.api_group, &info.api_version, &info.resource) {
                Some((cache, kind.to_string(), None))
            } else if let Some(mut client) = storage.clone() {
                match rest::resolve_dynamic_resource(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                    Ok(Some(resource)) => Some((cache, resource.kind, resource.conversion_webhook)),
                    Ok(None) => None,
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "watch: resolving the registered CRD-defined resource failed");
                        None
                    }
                }
            } else {
                None
            }
        } else if rest::resolve_kind(&info.api_group, &info.api_version, &info.resource).is_some() {
            None
        } else if let Some(mut client) = storage.clone() {
            match rest::resolve_dynamic_resource(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                Ok(Some(resource)) => {
                    let cache = cache_registry.spawn(client, &info.api_group, &info.api_version, &info.resource);
                    Some((cache, resource.kind, resource.conversion_webhook))
                }
                Ok(None) => None,
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "watch: resolving a possible CRD-defined resource failed");
                    None
                }
            }
        } else {
            None
        };

        if let Some((cache, kind, conversion_webhook)) = cache_and_kind {
            if !cache.has_synced() {
                if tokio::time::timeout(std::time::Duration::from_secs(30), cache.wait_until_synced()).await.is_err() {
                    warn!(path = %path_str, "watch: cache did not complete its initial LIST before the startup wait expired");
                    return Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(&path_str, "watch cache is not synchronized yet")));
                }
            }
            // Same real label/field selector parsing `rest::list` already
            // runs — a malformed selector is the client's fault, a `400`,
            // not a server failure, checked before the stream even starts
            // (matching `list`'s own "fail before doing any work" posture).
            let label_reqs = if info.label_selector.is_empty() {
                Vec::new()
            } else {
                match crate::cacher::selector::parse_label_selector(&info.label_selector) {
                    Ok(r) => r,
                    Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
                }
            };
            let field_reqs = if info.field_selector.is_empty() {
                Vec::new()
            } else {
                match crate::cacher::selector::parse_field_selector(&info.field_selector) {
                    Ok(r) => r,
                    Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
                }
            };
            if let Err(e) = crate::cacher::selector::validate_field_selector(&info.api_group, &info.resource, &field_reqs) {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string())));
            }
            let start_revision = resource_version_query(&query);
            let watch_options = match watch_options_query(&query) {
                Ok(options) => options,
                Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, error))),
            };
            match cache.watch_from(start_revision) {
                Ok((replay, rx)) => {
                    let group_version = if info.api_group.is_empty() { info.api_version.clone() } else { format!("{}/{}", info.api_group, info.api_version) };
                    let body = watch_response_body(
                        replay,
                        rx,
                        kind,
                        group_version,
                        label_reqs,
                        field_reqs,
                        storage.clone(),
                        info.api_group.clone(),
                        info.resource.clone(),
                        info.api_version.clone(),
                        wants_partial_metadata,
                        watch_options.allow_watch_bookmarks,
                        watch_options.timeout,
                        conversion_webhook,
                    );
                    // No explicit `Transfer-Encoding` header: hyper's own
                    // h1/h2 connection handling already frames a body with
                    // no known length correctly for whichever protocol
                    // this connection negotiated (chunked for h1, native
                    // DATA-frame streaming for h2, where the
                    // `Transfer-Encoding` header is actually forbidden by
                    // the HTTP/2 spec) — setting it here ourselves would
                    // be wrong for an h2 connection.
                    return Ok(Response::builder().status(StatusCode::OK).header("Content-Type", "application/json").body(body).unwrap());
                }
                Err(crate::cacher::store::Error::TooOld { .. }) => {
                    return Ok(json_response(StatusCode::GONE, &resource_expired_status(&path_str)));
                }
            }
        }
        // No cache registered (or spawnable) for this resource — falls
        // through to the echo stub below, same posture as every other
        // not-yet-served case in this handler.
    }

    // A resource-shaped request that reached this point targeted a verb or
    // subresource this server does not serve. Returning the request-info
    // echo with HTTP 200 makes kubectl treat an unsupported route as a
    // successful API response. Real kube-apiserver returns a Kubernetes
    // NotFound status for an unknown subresource, so keep the bring-up echo
    // limited to non-resource requests.
    if info.is_resource_request {
        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
    }

    // Surfaced for real observability (this is the only response shape
    // that ever includes it today), not consulted for any access-control
    // decision anywhere yet — there is no authorization (Group I) to
    // enforce it against. `rest::get`/`list` above don't take it either,
    // for the same reason: nothing yet checks a caller's identity before
    // serving a read.
    let user = identity.as_ref().map(|i| serde_json::json!({"username": i.name, "uid": i.uid, "groups": i.groups}));
    let value = serde_json::json!({
        "isResourceRequest": info.is_resource_request,
        "verb": info.verb,
        "apiPrefix": info.api_prefix,
        "apiGroup": info.api_group,
        "apiVersion": info.api_version,
        "namespace": info.namespace,
        "resource": info.resource,
        "subresource": info.subresource,
        "name": info.name,
        "user": user,
    });
