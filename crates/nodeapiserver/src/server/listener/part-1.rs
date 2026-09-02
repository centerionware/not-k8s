
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BoxedBody = http_body_util::combinators::BoxBody<hyper::body::Bytes, BoxError>;

type DynamicCacheState = HashMap<Vec<u8>, HashSet<crate::cacher::registry::ResourceKey>>;

#[derive(Debug, Clone, Default)]
struct AdmissionMetadata {
    warnings: Vec<String>,
    audit_failures: Vec<Value>,
}

type SharedAdmissionMetadata = Arc<Mutex<AdmissionMetadata>>;

fn record_admission_outcome(metadata: Option<&SharedAdmissionMetadata>, outcome: &admission::policy_enforcement::ValidationOutcome) {
    let Some(metadata) = metadata else {
        return;
    };
    let Ok(mut metadata) = metadata.lock() else {
        return;
    };
    metadata.warnings.extend(outcome.warnings.iter().cloned());
    metadata.audit_failures.extend(outcome.audit_failures.iter().cloned());
}

fn audit_annotations(metadata: &AdmissionMetadata) -> BTreeMap<String, String> {
    if metadata.audit_failures.is_empty() {
        return BTreeMap::new();
    }
    let value = serde_json::to_string(&metadata.audit_failures).unwrap_or_else(|_| "[]".to_string());
    BTreeMap::from([(admission::policy_enforcement::VALIDATION_FAILURE_AUDIT_ANNOTATION.to_string(), value)])
}

fn apply_admission_warnings(response: &mut Response<BoxedBody>, warnings: &[String]) {
    let warning_header = hyper::header::HeaderName::from_static("warning");
    for warning in warnings {
        // RFC 7234's warning-text is quoted; sanitize control characters so
        // a policy cannot inject a second header into the response.
        let escaped = warning.replace('\\', "\\\\").replace('"', "\\\"").replace('\r', " ").replace('\n', " ");
        let Ok(value) = hyper::header::HeaderValue::from_str(&format!("299 - \"{escaped}\"")) else {
            continue;
        };
        response.headers_mut().append(warning_header.clone(), value);
    }
}

fn crd_cache_keys(crd: &serde_json::Value) -> HashSet<crate::cacher::registry::ResourceKey> {
    crate::apiextensions::registry::discoverable_resources(std::iter::once(crd))
        .into_iter()
        .map(|resource| (resource.group, resource.version, resource.resource))
        .collect()
}

fn reconcile_crd_cache(
    storage: &StorageClient,
    registry: &crate::cacher::CacheRegistry,
    state: &mut DynamicCacheState,
    crd_key: Vec<u8>,
    crd: Option<&serde_json::Value>,
) {
    let previous = state.remove(&crd_key).unwrap_or_default();
    let desired = crd.map(crd_cache_keys).unwrap_or_default();

    for (group, version, resource) in previous.difference(&desired) {
        registry.remove(group, version, resource);
    }
    for (group, version, resource) in desired.difference(&previous) {
        registry.spawn(storage.clone(), group, version, resource);
    }

    if crd.is_some() {
        state.insert(crd_key, desired);
    }
}

async fn reconcile_crd_caches(
    storage: StorageClient,
    crd_cache: crate::cacher::SharedCache,
    registry: crate::cacher::CacheRegistry,
) {
    crd_cache.wait_until_synced().await;
    let (entries, mut events) = crd_cache.snapshot_and_watch();
    let mut state = DynamicCacheState::new();

    for (key, entry) in entries {
        match rest::decrypt_and_decode(&storage, "apiextensions.k8s.io", "customresourcedefinitions", &key, &entry.value) {
            Ok(crd) => reconcile_crd_cache(&storage, &registry, &mut state, key, Some(&crd)),
            Err(error) => warn!(error = ?error, "crd cache: failed to decode an initial CRD"),
        }
    }

    loop {
        match events.recv().await {
            Ok(event) => match event.kind {
                crate::cacher::EventKind::Added | crate::cacher::EventKind::Modified => {
                    match rest::decrypt_and_decode(&storage, "apiextensions.k8s.io", "customresourcedefinitions", &event.key, &event.value) {
                        Ok(crd) => reconcile_crd_cache(&storage, &registry, &mut state, event.key, Some(&crd)),
                        Err(error) => warn!(error = ?error, "crd cache: failed to decode a changed CRD"),
                    }
                }
                crate::cacher::EventKind::Deleted => {
                    reconcile_crd_cache(&storage, &registry, &mut state, event.key, None);
                }
                crate::cacher::EventKind::Bookmark => {}
            },
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(skipped, "crd cache: event stream lagged; rebuilding dynamic cache registrations");
                let (entries, next_events) = crd_cache.snapshot_and_watch();
                let current_keys: HashSet<Vec<u8>> = entries.iter().map(|(key, _)| key.clone()).collect();
                let stale_keys: Vec<Vec<u8>> = state.keys().filter(|key| !current_keys.contains(*key)).cloned().collect();
                for key in stale_keys {
                    reconcile_crd_cache(&storage, &registry, &mut state, key, None);
                }
                for (key, entry) in entries {
                    match rest::decrypt_and_decode(&storage, "apiextensions.k8s.io", "customresourcedefinitions", &key, &entry.value) {
                        Ok(crd) => reconcile_crd_cache(&storage, &registry, &mut state, key, Some(&crd)),
                        Err(error) => warn!(error = ?error, "crd cache: failed to decode a CRD while rebuilding registrations"),
                    }
                }
                events = next_events;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn body_from_bytes(bytes: Vec<u8>) -> BoxedBody {
    use http_body_util::{BodyExt, Full};
    Full::new(hyper::body::Bytes::from(bytes)).map_err(|never: std::convert::Infallible| match never {}).boxed()
}

/// Buffers a request's entire body into memory — fine for the object
/// sizes this build's own resources actually reach (real kube-apiserver
/// itself has no streaming write path either; every write is a single
/// decoded object). No size cap yet — a named, real gap, not a
/// forgotten one: real upstream enforces `--max-request-body-bytes`.
async fn read_body_bytes(req: Request<Incoming>) -> Result<Vec<u8>, hyper::Error> {
    use http_body_util::BodyExt;
    let collected = req.into_body().collect().await?;
    Ok(collected.to_bytes().to_vec())
}

fn json_response(status: StatusCode, value: &serde_json::Value) -> Response<BoxedBody> {
    json_response_with_content_type(status, value, "application/json")
}

fn json_response_with_content_type(status: StatusCode, value: &serde_json::Value, content_type: &str) -> Response<BoxedBody> {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder().status(status).header("Content-Type", content_type).body(body_from_bytes(bytes)).unwrap()
}

/// Real upstream's own `resourceVersion` query parameter for a `watch`
/// request — `path::RequestInfo` doesn't carry this (it's not part of
/// the URL *path* grammar `path::parse` ports, only the query string), so
/// this is read directly off the raw query the same ad hoc way
/// `content-type` is read off headers elsewhere in this function. `0` (the
/// same "unset"/"start from now" value `cacher::store::WatchCache::watch_from`
/// already treats `<= 0` as) for a missing or unparsable value.
fn resource_version_query(query: &str) -> i64 {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("resourceVersion="))
        .and_then(|v| urlencoding_decode(v).parse::<i64>().ok())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WatchOptions {
    allow_watch_bookmarks: bool,
    timeout: Option<std::time::Duration>,
}

/// Parses the two watch-only `ListOptions` this listener can honor without
/// changing the cache protocol. `allowWatchBookmarks` controls delivery of
/// the cache driver's synthetic bookmark events; `timeoutSeconds` bounds the
/// complete stream, including a quiet watch, just as upstream's watch
/// handler does. Zero means no server-side timeout.
fn watch_options_query(query: &str) -> Result<WatchOptions, &'static str> {
    let params = path::parse_query(query);
    let allow_watch_bookmarks = match params
        .iter()
        .find(|(key, _)| key == "allowWatchBookmarks")
    {
        None => false,
        Some((_, value)) => match value.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err("allowWatchBookmarks must be true or false"),
        },
    };
    let timeout = match params.iter().find(|(key, _)| key == "timeoutSeconds") {
        None => None,
        Some((_, value)) => {
            let seconds = value
                .parse::<u64>()
                .map_err(|_| "timeoutSeconds must be a non-negative integer")?;
            if seconds == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(seconds))
            }
        }
    };
    Ok(WatchOptions {
        allow_watch_bookmarks,
        timeout,
    })
}

/// The minimal `%XX`/`+` decoding a bare integer query value could ever
/// actually need — `resourceVersion` is always digits, so this only
/// exists to be defensive against a client that percent-encodes it
/// anyway (real browsers/`curl --data-urlencode` do this unconditionally
/// for some tooling); not a general URL-decoder.
fn urlencoding_decode(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('%') && !s.contains('+') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '+' => out.push(' '),
            '%' => {
                let hi = chars.next();
                let lo = chars.next();
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    if let Ok(byte) = u8::from_str_radix(&format!("{hi}{lo}"), 16) {
                        out.push(byte as char);
                        continue;
                    }
                }
                out.push('%');
            }
            other => out.push(other),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Real upstream's own Server-Side Apply media type
/// (`application/apply-patch+yaml`) — the one `rest::
/// patch_kind_for_content_type` deliberately doesn't recognize (its own
/// doc comment), since it isn't one of that function's three patch
/// kinds; this is the separate check that routes a `PATCH` into
/// `rest::server_side_apply` instead.
fn is_apply_patch_content_type(content_type: &str) -> bool {
    content_type.split(';').next().unwrap_or("").trim() == "application/apply-patch+yaml"
}

/// Real upstream's own required `?fieldManager=` query parameter for
/// Server-Side Apply — `path::RequestInfo` doesn't carry it, same reason
/// `resource_version_query` above doesn't come from there either.
/// `None` when absent, so the caller can reject with a real `400` rather
/// than inventing a manager name.
fn field_manager_query(query: &str) -> Option<String> {
    path::parse_query(query).into_iter().find(|(k, _)| k == "fieldManager").map(|(_, v)| v).filter(|value| !value.is_empty())
}

/// Real upstream's own `?force=` query parameter — Server-Side Apply's
/// conflict-override flag.
fn force_query(query: &str) -> bool {
    path::parse_query(query).iter().any(|(k, v)| k == "force" && v == "true")
}

/// Parses the write-only `dryRun` query option. Kubernetes currently defines
/// one value, `All`; accepting anything else would make a misspelled option
/// look like a successful persisted write.
fn dry_run_query(query: &str) -> Result<bool, &'static str> {
    let Some((_, value)) = path::parse_query(query).into_iter().find(|(key, _)| key == "dryRun") else {
        return Ok(false);
    };
    match value.as_str() {
        "All" => Ok(true),
        _ => Err("dryRun must be All"),
    }
}

fn is_authorization_review(info: &path::RequestInfo) -> bool {
    (info.api_group == "authorization.k8s.io"
        && matches!(
            info.resource.as_str(),
            "subjectaccessreviews"
                | "selfsubjectaccessreviews"
                | "localsubjectaccessreviews"
                | "selfsubjectrulesreviews"
        ))
        || (info.api_group == "authentication.k8s.io" && info.resource == "selfsubjectreviews")
}

fn should_run_local_authorization(
    info: &path::RequestInfo,
    enforce_rbac: bool,
    authorization_webhook_allowed: bool,
) -> bool {
    enforce_rbac
        && !authorization_webhook_allowed
        && info.is_resource_request
        && !is_authorization_review(info)
}

fn delete_preconditions(value: Option<&serde_json::Value>) -> Result<Option<rest::DeletePreconditions>, &'static str> {
    let Some(preconditions) = value.and_then(|value| value.get("preconditions")) else {
        return Ok(None);
    };
    let Some(preconditions) = preconditions.as_object() else {
        return Err("metadata.preconditions must be an object");
    };
    let string_field = |name: &str| -> Result<Option<String>, &'static str> {
        match preconditions.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => value.as_str().map(|value| Some(value.to_string())).ok_or("delete preconditions must be strings"),
        }
    };
    Ok(Some(rest::DeletePreconditions {
        resource_version: string_field("resourceVersion")?,
        uid: string_field("uid")?,
    }))
}

/// Real upstream's own `Conflict` shape for a Server-Side Apply
/// ownership conflict — `reason: "Conflict"`, `code: 409`. Same "real
/// subset, not the full type" posture every other `Status` builder in
/// this module takes: real upstream's own structured
/// `Status.details.causes` (one `field.ManagedFieldsConflict` entry per
/// conflicting manager) isn't built, `message` joins them into one
/// human-readable string instead.
fn ssa_conflict_status(path_str: &str, conflicts: &[crate::patch::updater::Conflict]) -> serde_json::Value {
    let detail = conflicts.iter().map(|c| format!("\"{}\" already owns: {}", c.manager, c.fields.to_json())).collect::<Vec<_>>().join("; ");
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: conflict with existing field manager(s): {detail}"),
        "reason": "Conflict",
        "details": {},
        "code": 409,
    })
}

/// Real upstream's own minimal `Status` shape for the `410 Gone` a watch
/// whose `resourceVersion` has fallen out of the cache's retained history
/// window gets — `reason: "Gone"`, `code: 410`, matching real
/// kube-apiserver's own `errors.NewResourceExpired` (the signal every
/// real `client-go` informer relists on).
fn resource_expired_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: the requested resource version is too old — relist required"),
        "reason": "Gone",
        "details": {},
        "code": 410,
    })
}

/// Encodes one `WatchEvent` as a single newline-terminated JSON document —
/// the same framing real `client-go`'s own `StreamWatcher`/
/// `restclientwatch.NewDecoder` reads a JSON watch response with
/// (`io.Reader`-based JSON decoders read one value at a time regardless of
/// a trailing newline, but emitting one keeps every line independently
/// parseable by simpler line-oriented tooling like `curl | jq -c`, and
/// matches what a real kube-apiserver response looks like on the wire).
/// `None` when [`crate::server::watch_event::to_watch_event_json`] itself
/// returns `None` (the one honest, narrow case: a `Deleted` event for a
/// key this cache never held a value for — see that module's own doc
/// comment) — the event is silently skipped from the stream rather than
/// breaking it, since there is nothing real to report for it.
fn encode_watch_event(
    event: &crate::cacher::store::WatchEvent,
    kind: &str,
    api_version: &str,
    storage: Option<&StorageClient>,
    group: &str,
    resource: &str,
    version: &str,
    partial_metadata: bool,
) -> Option<Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>> {
    match crate::server::watch_event::to_watch_event_json(event, kind, api_version, storage, group, resource) {
        None => None,
        Some(Ok(mut event_json)) => {
            if partial_metadata {
                if let Some(object) = event_json.get_mut("object") {
                    *object = crate::codec::partial_metadata::object(object);
                }
            }
            let mut bytes = serde_json::to_vec(&event_json).unwrap_or_default();
            bytes.push(b'\n');
            // Group M: `apiserver_watch_events_total` -- real upstream's
            // own increment point too (`metrics.go`'s own `WatchEvents.
            // WithLabelValues(...).Inc()`, called once per event actually
            // written to a watch client's connection, not per event this
            // build merely considered and filtered out).
            metrics::record_watch_event(group, version, resource);
            Some(Ok(hyper::body::Frame::data(hyper::body::Bytes::from(bytes))))
        }
        Some(Err(e)) => Some(Err(Box::new(e) as BoxError)),
    }
}

async fn encode_watch_event_with_conversion(
    event: &crate::cacher::store::WatchEvent,
    kind: &str,
    api_version: &str,
    storage: Option<StorageClient>,
    group: &str,
    resource: &str,
    version: &str,
    partial_metadata: bool,
    conversion_webhook: Option<crate::apiextensions::registry::ConversionWebhook>,
) -> Option<Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>> {
    let mut storage = storage;
    match crate::server::watch_event::to_watch_event_json_with_conversion(
        event,
        kind,
        api_version,
        storage.as_mut(),
        group,
        resource,
        conversion_webhook.as_ref(),
    )
    .await
    {
        None => None,
        Some(Ok(mut event_json)) => {
            if partial_metadata {
                if let Some(object) = event_json.get_mut("object") {
                    *object = crate::codec::partial_metadata::object(object);
                }
            }
            let mut bytes = serde_json::to_vec(&event_json).unwrap_or_default();
            bytes.push(b'\n');
            metrics::record_watch_event(group, version, resource);
            Some(Ok(hyper::body::Frame::data(hyper::body::Bytes::from(bytes))))
        }
        Some(Err(error)) => Some(Err(Box::new(error) as BoxError)),
    }
}

type WatchEventStream = Pin<Box<dyn tokio_stream::Stream<Item = crate::cacher::store::WatchEvent> + Send + Sync>>;
type WatchFrameFuture = Pin<Box<dyn Future<Output = Option<Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>>> + Send>>;

struct ConversionWatchState {
    events: WatchEventStream,
    pending: Option<WatchFrameFuture>,
    kind: String,
    api_version: String,
    storage: Option<StorageClient>,
    group: String,
    resource: String,
    version: String,
    partial_metadata: bool,
    conversion_webhook: Option<crate::apiextensions::registry::ConversionWebhook>,
}

struct ConversionWatchStream {
    state: Arc<Mutex<ConversionWatchState>>,
}

impl tokio_stream::Stream for ConversionWatchStream {
    type Item = Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut state = self.state.lock().expect("conversion watch state lock poisoned");
        loop {
            if state.pending.is_some() {
                let poll = state.pending.as_mut().expect("pending conversion future exists").as_mut().poll(cx);
                match poll {
                    Poll::Ready(result) => {
                        state.pending = None;
                        if let Some(result) = result {
                            return Poll::Ready(Some(result));
                        }
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            let event = match state.events.as_mut().poll_next(cx) {
                Poll::Ready(Some(event)) => event,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            };
            let kind = state.kind.clone();
            let api_version = state.api_version.clone();
            let storage = state.storage.clone();
            let group = state.group.clone();
            let resource = state.resource.clone();
            let version = state.version.clone();
            let partial_metadata = state.partial_metadata;
            let conversion_webhook = state.conversion_webhook.clone();
            state.pending = Some(Box::pin(async move {
                encode_watch_event_with_conversion(
                    &event,
                    &kind,
                    &api_version,
                    storage,
                    &group,
                    &resource,
                    &version,
                    partial_metadata,
                    conversion_webhook,
                )
                .await
            }));
        }
    }
}

/// The real streaming `watch` response body: every already-retained
/// history event past `start_revision` (`replay`), then every live event
/// as it arrives on `rx`, each encoded by [`encode_watch_event`]. A
/// `broadcast::Receiver::recv()` `Lagged` error (the watcher fell behind
/// the channel's bounded capacity) ends the stream rather than skipping
/// silently past the gap — real kube-apiserver's own posture for a
/// watcher that falls too far behind: close the connection, the client's
/// own `client-go` Reflector relists. `StreamBody`/`Frame` come from
/// `http_body_util`/`hyper::body` — `BoxedBody` (a boxed `http_body::Body`
/// trait object) is what lets this coexist with every other, non-streaming
/// `Response<BoxedBody>` this listener already returns; hyper's own h1/h2
/// connection handling picks chunked transfer-encoding (h1) or native
/// framing (h2) automatically for a body with no known `Content-Length`,
/// no explicit opt-in needed here.
/// `true` when `event` should reach the client — real upstream's own
/// `WatchCache`/`cacheWatcher` narrows a watch to matching objects too,
/// not just the initial `LIST`'s own selector filtering. `Bookmark`
/// events and any event this cache never retained a value for (an old
/// `Deleted` with no captured prior state) always pass through — there's
/// no object to test a selector against, the same "nothing to filter"
/// case `label_reqs.is_empty() && field_reqs.is_empty()` short-circuits.
/// A value this build can't decode also passes through rather than
/// being silently dropped — filtering a watch is a narrowing, never a
/// hiding, mechanism; a real decode failure is a `warn!`, not a
/// swallowed event.
fn watch_event_matches_selector(
    event: &crate::cacher::store::WatchEvent,
    label_reqs: &[crate::cacher::selector::Requirement],
    field_reqs: &[crate::cacher::selector::FieldRequirement],
    storage: Option<&StorageClient>,
    group: &str,
    resource: &str,
) -> bool {
    if label_reqs.is_empty() && field_reqs.is_empty() {
        return true;
    }
    if event.value.is_empty() {
        return true;
    }
    let decoded = match storage {
        Some(s) => rest::decrypt_and_decode(s, group, resource, &event.key, &event.value),
        None => rest::decode_stored_object(&event.value).map_err(rest::Error::from),
    };
    match decoded {
        Ok(object) => crate::cacher::selector::object_matches(&object, label_reqs, field_reqs),
        Err(e) => {
            warn!(error = ?e, "watch: failed to decode a cached value for selector filtering; letting the event through unfiltered");
            true
        }
    }
}

/// The real streaming `watch` response body: every already-retained
/// history event past `start_revision` (`replay`), then every live event
/// as it arrives on `rx`, each filtered by [`watch_event_matches_selector`]
/// (the same real label/field selector `LIST` already applies, now
/// applied to a live stream too) and encoded by [`encode_watch_event`]. A
/// `broadcast::Receiver::recv()` `Lagged` error (the watcher fell behind
/// the channel's bounded capacity) ends the stream rather than skipping
/// silently past the gap — real kube-apiserver's own posture for a
/// watcher that falls too far behind: close the connection, the client's
/// own `client-go` Reflector relists. `StreamBody`/`Frame` come from
/// `http_body_util`/`hyper::body` — `BoxedBody` (a boxed `http_body::Body`
/// trait object) is what lets this coexist with every other, non-streaming
/// `Response<BoxedBody>` this listener already returns; hyper's own h1/h2
/// connection handling picks chunked transfer-encoding (h1) or native
/// framing (h2) automatically for a body with no known `Content-Length`,
/// no explicit opt-in needed here.
fn watch_response_body(
    replay: Vec<crate::cacher::store::WatchEvent>,
    rx: tokio::sync::broadcast::Receiver<crate::cacher::store::WatchEvent>,
    kind: String,
    api_version: String,
    label_reqs: Vec<crate::cacher::selector::Requirement>,
    field_reqs: Vec<crate::cacher::selector::FieldRequirement>,
    storage: Option<StorageClient>,
    group: String,
    resource: String,
    version: String,
    partial_metadata: bool,
    allow_watch_bookmarks: bool,
    timeout: Option<std::time::Duration>,
    conversion_webhook: Option<crate::apiextensions::registry::ConversionWebhook>,
) -> BoxedBody {
    use http_body_util::{BodyExt, StreamBody};
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt;

    let replay_stream = tokio_stream::iter(replay);
    let live_stream = BroadcastStream::new(rx).map_while(|res| res.ok());
    let events = replay_stream
        .chain(live_stream)
        .filter(move |event| allow_watch_bookmarks || event.kind != crate::cacher::store::EventKind::Bookmark);
    let events: WatchEventStream = if let Some(timeout) = timeout {
        Box::pin(futures::StreamExt::take_until(
            events,
            tokio::time::sleep(timeout),
        ))
    } else {
        Box::pin(events)
    };
    // Cloned once per closure (`StorageClient` wraps a cheap-to-clone
    // `tonic::transport::Channel`, same posture every other real call
    // site in this crate already takes) — `filter`/`filter_map` each need
    // their own `'static`-owned copy of the encryption-lookup context.
    let (storage_for_filter, group_for_filter, resource_for_filter) = (storage.clone(), group.clone(), resource.clone());
    let filtered = events.filter(move |event| watch_event_matches_selector(event, &label_reqs, &field_reqs, storage_for_filter.as_ref(), &group_for_filter, &resource_for_filter));
    if conversion_webhook.is_none() {
        let frames = filtered.filter_map(move |event| {
            encode_watch_event(&event, &kind, &api_version, storage.as_ref(), &group, &resource, &version, partial_metadata)
        });
        return StreamBody::new(frames).boxed();
    }

    let events: WatchEventStream = Box::pin(filtered);
    let stream = ConversionWatchStream {
        state: Arc::new(Mutex::new(ConversionWatchState {
            events,
            pending: None,
            kind,
            api_version,
            storage,
            group,
            resource,
            version,
            partial_metadata,
            conversion_webhook,
        })),
    };
    StreamBody::new(stream).boxed()
}
