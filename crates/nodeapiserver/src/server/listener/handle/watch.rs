macro_rules! handle_watch {
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
    // Group D/E: real `WATCH`, served purely from an already-registered
    // `cacher::CacheRegistry` cache. A live cache already holds
    // everything the read side of this handler needs (a snapshot to
    // replay from, a live event subscription). If a resource has no
    // registered cache, the handler returns a real Kubernetes error below
    // rather than claiming a successful watch this build cannot serve.
    //
    // Group I: RBAC, gated by `$enforce_rbac` same as every other verb —
    // resolved against a fresh `$storage.clone()` (cheap — a
    // `tonic::transport::Channel` clone, same as every other real call
    // site), since `watch` doesn't otherwise need `$storage`/`client` at
    // all. Unlike a request this build can *choose* to allow when RBAC is
    // off, "enforcement is on but there's no $storage connection to
    // resolve rules against" fails closed (`500`), never silently
    // degrading to "allow" — the whole reason `$enforce_rbac` exists is to
    // guarantee a denial-capable policy actually ran. Group J admission
    // intentionally does **not** gate `watch` here, matching real
    // upstream's own posture (admission never runs on a read, whatever
    // the verb) — not a gap.
    if $is_watch {
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
            Vec<String>,
        )> = if let Some(cache) = $cache_registry.get(&$info.api_group, &$info.api_version, &$info.resource) {
            if let Some(kind) = rest::resolve_kind(&$info.api_group, &$info.api_version, &$info.resource) {
                Some((cache, kind.to_string(), None, Vec::new()))
            } else if let Some(mut client) = $storage.clone() {
                match rest::resolve_dynamic_resource(&mut client, &$info.api_group, &$info.api_version, &$info.resource).await {
                    Ok(Some(resource)) => Some((cache, resource.kind, resource.conversion_webhook, resource.selectable_fields)),
                    Ok(None) => None,
                    Err(e) => {
                        warn!(path = %$path_str, error = ?e, "watch: resolving the registered CRD-defined resource failed");
                        None
                    }
                }
            } else {
                None
            }
        } else if rest::resolve_kind(&$info.api_group, &$info.api_version, &$info.resource).is_some() {
            None
        } else if let Some(mut client) = $storage.clone() {
            match rest::resolve_dynamic_resource(&mut client, &$info.api_group, &$info.api_version, &$info.resource).await {
                Ok(Some(resource)) => {
                    let cache = $cache_registry.spawn(client, &$info.api_group, &$info.api_version, &$info.resource);
                    Some((cache, resource.kind, resource.conversion_webhook, resource.selectable_fields))
                }
                Ok(None) => None,
                Err(e) => {
                    warn!(path = %$path_str, error = ?e, "watch: resolving a possible CRD-defined resource failed");
                    None
                }
            }
        } else {
            None
        };

        if let Some((cache, kind, conversion_webhook, selectable_fields)) = cache_and_kind {
            if !cache.has_synced() {
                if tokio::time::timeout(std::time::Duration::from_secs(30), cache.wait_until_synced()).await.is_err() {
                    warn!(path = %$path_str, "watch: cache did not complete its initial LIST before the startup wait expired");
                    return Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(&$path_str, "watch cache is not synchronized yet")));
                }
            }
            // Same real label/field selector parsing `rest::list` already
            // runs — a malformed selector is the client's fault, a `400`,
            // not a server failure, checked before the stream even starts
            // (matching `list`'s own "fail before doing any work" posture).
            let label_reqs = if $info.label_selector.is_empty() {
                Vec::new()
            } else {
                match crate::cacher::selector::parse_label_selector(&$info.label_selector) {
                    Ok(r) => r,
                    Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string()))),
                }
            };
            let field_reqs = if $info.field_selector.is_empty() {
                Vec::new()
            } else {
                match crate::cacher::selector::parse_field_selector(&$info.field_selector) {
                    Ok(r) => r,
                    Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string()))),
                }
            };
            if let Err(e) = crate::cacher::selector::validate_field_selector_with_additional_fields(&$info.api_group, &$info.resource, &field_reqs, &selectable_fields) {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, &e.to_string())));
            }
            let start_revision = resource_version_query(&$query);
            let watch_options = match watch_options_query(&$query) {
                Ok(options) => options,
                Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&$path_str, error))),
            };
            // Newer client-go informers use the streaming-list form of WATCH:
            // they request the current objects as synthetic ADDED events and
            // do not consider the informer synchronized until the server
            // sends a BOOKMARK annotated `k8s.io/initial-events-end=true`.
            // Take the cache snapshot before subscribing; `watch_from` then
            // replays any event racing that snapshot, preserving the normal
            // LIST-then-WATCH handoff without a gap.
            let initial_events = if watch_options.send_initial_events {
                let (entries, revision) = cache.list();
                let prefix = crate::$storage::keys::list_prefix(&$info.api_group, &$info.resource, Some(&$info.namespace)).into_bytes();
                let events = entries
                    .into_iter()
                    .filter(|(key, _)| key.starts_with(&prefix))
                    .map(|(key, entry)| crate::cacher::store::WatchEvent {
                        kind: crate::cacher::store::EventKind::Added,
                        key,
                        value: entry.value,
                        revision,
                    })
                    .collect();
                Some((events, revision))
            } else {
                None
            };
            let watch_start_revision = initial_events.as_ref().map(|(_, revision)| *revision).unwrap_or(start_revision);
            let watch_result = if initial_events.is_some() {
                cache.watch_from_snapshot(watch_start_revision)
            } else {
                cache.watch_from(watch_start_revision)
            };
            match watch_result {
                Ok((replay, rx)) => {
                    let group_version = if $info.api_group.is_empty() { $info.api_version.clone() } else { format!("{}/{}", $info.api_group, $info.api_version) };
                    let body = if initial_events.is_some() {
                        watch_response_body_with_initial_events(
                            replay,
                            rx,
                            kind,
                            group_version,
                            label_reqs,
                            field_reqs,
                            $storage.clone(),
                            $info.api_group.clone(),
                            $info.resource.clone(),
                            $info.api_version.clone(),
                            $wants_partial_metadata,
                            watch_options.allow_watch_bookmarks,
                            watch_options.timeout,
                            conversion_webhook,
                            initial_events,
                        )
                    } else {
                        watch_response_body(
                            replay,
                            rx,
                            kind,
                            group_version,
                            label_reqs,
                            field_reqs,
                            $storage.clone(),
                            $info.api_group.clone(),
                            $info.resource.clone(),
                            $info.api_version.clone(),
                            $wants_partial_metadata,
                            watch_options.allow_watch_bookmarks,
                            watch_options.timeout,
                            conversion_webhook,
                        )
                    };
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
                    // Once authentication/authorization and the resource
                    // version have been accepted, upstream keeps the watch
                    // at HTTP 200 and reports an expired version as an
                    // in-band ERROR event. kube-rs uses that event to reset
                    // its watcher to a fresh LIST; returning HTTP 410 here
                    // leaves a resumed watcher retrying the same stale RV.
                    return Ok(watch_resource_expired_response(&$path_str));
                }
            }
        }
        // No cache registered (or spawnable) for this resource — falls
        // through to the real not-found response below.
    }

    // A resource-shaped request that reached this point targeted a verb or
    // subresource this server does not serve. Real kube-apiserver returns a
    // Kubernetes NotFound status for an unknown subresource.
    if $info.is_resource_request {
        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)));
    }

    // Unknown non-resource paths are also real API errors. The old bring-up
    // echo made a typo look like a successful request and was incompatible
    // with kubectl/client-go error handling.
    Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&$path_str)))
    }};
}
