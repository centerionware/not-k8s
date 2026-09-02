type DynamicCacheState = HashMap<Vec<u8>, HashSet<crate::cacher::registry::ResourceKey>>;

// ResourceQuota usage is derived from a live LIST immediately before the
// object CREATE. Serialize namespaced creates in this process so two
// simultaneous requests cannot both observe the same pre-create usage and
// exceed a quota. The nodestore transaction still supplies the final
// object-level uniqueness/concurrency check; this lock closes the quota's
// separate read/check/create window for a single nodeapiserver process.
static RESOURCE_QUOTA_ADMISSION_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, Default)]
struct AdmissionMetadata {
    warnings: Vec<String>,
    audit_failures: Vec<Value>,
}
type SharedAdmissionMetadata = Arc<Mutex<AdmissionMetadata>>;

#[derive(Clone)]
struct AuditRequestBodyCapture(Arc<Mutex<Option<Vec<u8>>>>);

#[derive(Clone, Copy)]
struct RequestBodyLimit(usize);

#[derive(Debug)]
enum BodyReadError {
    TooLarge { limit: usize },
    Hyper(hyper::Error),
}

impl std::fmt::Display for BodyReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { limit } => {
                write!(formatter, "request body exceeds the {limit}-byte limit")
            }
            Self::Hyper(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for BodyReadError {}

impl From<hyper::Error> for BodyReadError {
    fn from(error: hyper::Error) -> Self {
        Self::Hyper(error)
    }
}

fn record_admission_outcome(
    metadata: Option<&SharedAdmissionMetadata>,
    outcome: &admission::policy_enforcement::ValidationOutcome,
) {
    let Some(metadata) = metadata else {
        return;
    };
    let Ok(mut metadata) = metadata.lock() else {
        return;
    };
    metadata.warnings.extend(outcome.warnings.iter().cloned());
    metadata
        .audit_failures
        .extend(outcome.audit_failures.iter().cloned());
}

fn audit_annotations(metadata: &AdmissionMetadata) -> BTreeMap<String, String> {
    if metadata.audit_failures.is_empty() {
        return BTreeMap::new();
    }
    let value =
        serde_json::to_string(&metadata.audit_failures).unwrap_or_else(|_| "[]".to_string());
    BTreeMap::from([(
        admission::policy_enforcement::VALIDATION_FAILURE_AUDIT_ANNOTATION.to_string(),
        value,
    )])
}

fn apply_admission_warnings(response: &mut Response<BoxedBody>, warnings: &[String]) {
    let warning_header = hyper::header::HeaderName::from_static("warning");
    for warning in warnings {
        // RFC 7234's warning-text is quoted; sanitize control characters so
        // a policy cannot inject a second header into the response.
        let escaped = warning
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\r', " ")
            .replace('\n', " ");
        let Ok(value) = hyper::header::HeaderValue::from_str(&format!("299 - \"{escaped}\""))
        else {
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
        match rest::decrypt_and_decode(
            &storage,
            "apiextensions.k8s.io",
            "customresourcedefinitions",
            &key,
            &entry.value,
        ) {
            Ok(crd) => reconcile_crd_cache(&storage, &registry, &mut state, key, Some(&crd)),
            Err(error) => warn!(error = ?error, "crd cache: failed to decode an initial CRD"),
        }
    }

    loop {
        match events.recv().await {
            Ok(event) => match event.kind {
                crate::cacher::EventKind::Added | crate::cacher::EventKind::Modified => {
                    match rest::decrypt_and_decode(
                        &storage,
                        "apiextensions.k8s.io",
                        "customresourcedefinitions",
                        &event.key,
                        &event.value,
                    ) {
                        Ok(crd) => reconcile_crd_cache(
                            &storage,
                            &registry,
                            &mut state,
                            event.key,
                            Some(&crd),
                        ),
                        Err(error) => {
                            warn!(error = ?error, "crd cache: failed to decode a changed CRD")
                        }
                    }
                }
                crate::cacher::EventKind::Deleted => {
                    reconcile_crd_cache(&storage, &registry, &mut state, event.key, None);
                }
                crate::cacher::EventKind::Bookmark => {}
            },
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "crd cache: event stream lagged; rebuilding dynamic cache registrations"
                );
                let (entries, next_events) = crd_cache.snapshot_and_watch();
                let current_keys: HashSet<Vec<u8>> =
                    entries.iter().map(|(key, _)| key.clone()).collect();
                let stale_keys: Vec<Vec<u8>> = state
                    .keys()
                    .filter(|key| !current_keys.contains(*key))
                    .cloned()
                    .collect();
                for key in stale_keys {
                    reconcile_crd_cache(&storage, &registry, &mut state, key, None);
                }
                for (key, entry) in entries {
                    match rest::decrypt_and_decode(
                        &storage,
                        "apiextensions.k8s.io",
                        "customresourcedefinitions",
                        &key,
                        &entry.value,
                    ) {
                        Ok(crd) => {
                            reconcile_crd_cache(&storage, &registry, &mut state, key, Some(&crd))
                        }
                        Err(error) => {
                            warn!(error = ?error, "crd cache: failed to decode a CRD while rebuilding registrations")
                        }
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
    Full::new(hyper::body::Bytes::from(bytes))
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed()
}

/// Buffers a request's body into memory while enforcing the listener's
/// configured maximum. The frame-by-frame check also covers chunked bodies,
/// for which `Content-Length` cannot provide an early bound.
async fn read_body_bytes(req: Request<Incoming>) -> Result<Vec<u8>, BodyReadError> {
    use http_body_util::BodyExt;
    let limit = req
        .extensions()
        .get::<RequestBodyLimit>()
        .map_or(usize::MAX, |limit| limit.0);
    let capture = req
        .extensions()
        .get::<AuditRequestBodyCapture>()
        .map(|capture| capture.0.clone());
    let mut body = req.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if data.len() > limit.saturating_sub(bytes.len()) {
            return Err(BodyReadError::TooLarge { limit });
        }
        bytes.extend_from_slice(&data);
    }
    if let Some(capture) = capture {
        if let Ok(mut captured) = capture.lock() {
            *captured = Some(bytes.clone());
        }
    }
    Ok(bytes)
}

fn body_read_error_response(path_str: &str, error: &BodyReadError) -> Response<BoxedBody> {
    match error {
        BodyReadError::TooLarge { limit } => json_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            &request_entity_too_large_status(path_str, *limit),
        ),
        BodyReadError::Hyper(_) => json_response(
            StatusCode::BAD_REQUEST,
            &bad_request_status(path_str, "request body could not be read"),
        ),
    }
}

/// Captures a bounded, already-materialized response body for audit
/// `RequestResponse` events while returning the same bytes to the client.
/// Streaming responses and bodies larger than the request limit remain
/// uncaptured so audit logging cannot turn a normal response into an
/// unbounded second buffer.
async fn capture_response_object(
    response: Response<BoxedBody>,
    max_bytes: usize,
) -> (Response<BoxedBody>, Option<Value>) {
    use http_body::Body as _;
    use http_body_util::BodyExt;

    let (parts, body) = response.into_parts();
    let Some(size) = body.size_hint().exact() else {
        return (Response::from_parts(parts, body), None);
    };
    if size > max_bytes as u64 {
        return (Response::from_parts(parts, body), None);
    }
    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            warn!(error = ?error, "nodeapiserver: failed to capture response body for audit");
            return (
                Response::from_parts(parts, body_from_bytes(Vec::new())),
                None,
            );
        }
    };
    let object = decode_audit_object(&bytes, content_type.as_deref());
    (
        Response::from_parts(parts, body_from_bytes(bytes.to_vec())),
        object,
    )
}

fn decode_audit_object(bytes: &[u8], content_type: Option<&str>) -> Option<Value> {
    if bytes.is_empty() {
        return None;
    }
    match content_type
        .and_then(negotiation::content_type)
        .unwrap_or(negotiation::Format::Json)
    {
        negotiation::Format::Json => crate::codec::json::decode(bytes).ok(),
        negotiation::Format::Yaml => crate::codec::yaml::decode(bytes).ok(),
        negotiation::Format::Protobuf => None,
    }
}

fn json_response(status: StatusCode, value: &serde_json::Value) -> Response<BoxedBody> {
    json_response_with_content_type(status, value, "application/json")
}

fn json_response_with_content_type(
    status: StatusCode,
    value: &serde_json::Value,
    content_type: &str,
) -> Response<BoxedBody> {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .body(body_from_bytes(bytes))
        .unwrap()
}

fn scale_outcome_response(path: &str, outcome: rest::ScaleOutcome) -> Response<BoxedBody> {
    match outcome {
        rest::ScaleOutcome::Found(scale) | rest::ScaleOutcome::Updated(scale) => {
            json_response(StatusCode::OK, &scale)
        }
        rest::ScaleOutcome::UnknownResource | rest::ScaleOutcome::ObjectNotFound => {
            json_response(StatusCode::NOT_FOUND, &not_found_status(path))
        }
        rest::ScaleOutcome::MissingResourceVersion => json_response(
            StatusCode::BAD_REQUEST,
            &bad_request_status(path, "metadata.resourceVersion is required"),
        ),
        rest::ScaleOutcome::Conflict => json_response(StatusCode::CONFLICT, &conflict_status(path)),
        rest::ScaleOutcome::Invalid(violations) => json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            &invalid_status(path, &violations),
        ),
    }
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
    send_initial_events: bool,
    timeout: Option<std::time::Duration>,
}

/// Parses the watch-only `ListOptions` this listener can honor without
/// changing the cache protocol. `allowWatchBookmarks` controls delivery of
/// the cache driver's synthetic bookmark events; `sendInitialEvents` enables
/// the streaming-list handshake used by newer client-go informers; and
/// `timeoutSeconds` bounds the complete stream, including a quiet watch, just
/// as upstream's watch handler does. Zero means no server-side timeout.
fn watch_options_query(query: &str) -> Result<WatchOptions, &'static str> {
    let params = path::parse_query(query);
    let allow_watch_bookmarks = match params.iter().find(|(key, _)| key == "allowWatchBookmarks") {
        None => false,
        Some((_, value)) => match value.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err("allowWatchBookmarks must be true or false"),
        },
    };
    let send_initial_events = match params.iter().find(|(key, _)| key == "sendInitialEvents") {
        None => false,
        Some((_, value)) => match value.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err("sendInitialEvents must be true or false"),
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
        send_initial_events,
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
    path::parse_query(query)
        .into_iter()
        .find(|(k, _)| k == "fieldManager")
        .map(|(_, v)| v)
        .filter(|value| !value.is_empty())
}

/// Real upstream's own `?force=` query parameter — Server-Side Apply's
/// conflict-override flag.
fn force_query(query: &str) -> bool {
    path::parse_query(query)
        .iter()
        .any(|(k, v)| k == "force" && v == "true")
}

/// Parses the write-only `dryRun` query option. Kubernetes currently defines
/// one value, `All`; accepting anything else would make a misspelled option
/// look like a successful persisted write.
fn dry_run_query(query: &str) -> Result<bool, &'static str> {
    let Some((_, value)) = path::parse_query(query)
        .into_iter()
        .find(|(key, _)| key == "dryRun")
    else {
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
    enforce_rbac && !authorization_webhook_allowed && !is_authorization_review(info)
}

fn delete_preconditions(
    value: Option<&serde_json::Value>,
) -> Result<Option<rest::DeletePreconditions>, &'static str> {
    let Some(preconditions) = value.and_then(|value| value.get("preconditions")) else {
        return Ok(None);
    };
    let Some(preconditions) = preconditions.as_object() else {
        return Err("metadata.preconditions must be an object");
    };
    let string_field = |name: &str| -> Result<Option<String>, &'static str> {
        match preconditions.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => value
                .as_str()
                .map(|value| Some(value.to_string()))
                .ok_or("delete preconditions must be strings"),
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
fn ssa_conflict_status(
    path_str: &str,
    conflicts: &[crate::patch::updater::Conflict],
) -> serde_json::Value {
    let detail = conflicts
        .iter()
        .map(|c| format!("\"{}\" already owns: {}", c.manager, c.fields.to_json()))
        .collect::<Vec<_>>()
        .join("; ");
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
    initial_events_end: bool,
) -> Option<Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>> {
    match crate::server::watch_event::to_watch_event_json(
        event,
        kind,
        api_version,
        storage,
        group,
        resource,
    ) {
        None => None,
        Some(Ok(mut event_json)) => {
            if partial_metadata {
                if let Some(object) = event_json.get_mut("object") {
                    *object = crate::codec::partial_metadata::object(object);
                }
            }
            if initial_events_end {
                mark_initial_events_end(&mut event_json);
            }
            let mut bytes = serde_json::to_vec(&event_json).unwrap_or_default();
            bytes.push(b'\n');
            // Group M: `apiserver_watch_events_total` -- real upstream's
            // own increment point too (`metrics.go`'s own `WatchEvents.
            // WithLabelValues(...).Inc()`, called once per event actually
            // written to a watch client's connection, not per event this
            // build merely considered and filtered out).
            metrics::record_watch_event(group, version, resource);
            Some(Ok(hyper::body::Frame::data(hyper::body::Bytes::from(
                bytes,
            ))))
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
    initial_events_end: bool,
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
            if initial_events_end {
                mark_initial_events_end(&mut event_json);
            }
            let mut bytes = serde_json::to_vec(&event_json).unwrap_or_default();
            bytes.push(b'\n');
            metrics::record_watch_event(group, version, resource);
            Some(Ok(hyper::body::Frame::data(hyper::body::Bytes::from(
                bytes,
            ))))
        }
        Some(Err(error)) => Some(Err(Box::new(error) as BoxError)),
    }
}

fn mark_initial_events_end(event_json: &mut Value) {
    let Some(object) = event_json.get_mut("object").and_then(Value::as_object_mut) else {
        return;
    };
    let metadata = object
        .entry("metadata")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !metadata.is_object() {
        *metadata = Value::Object(serde_json::Map::new());
    }
    let Some(metadata) = metadata.as_object_mut() else {
        return;
    };
    let annotations = metadata
        .entry("annotations")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !annotations.is_object() {
        *annotations = Value::Object(serde_json::Map::new());
    }
    annotations["k8s.io/initial-events-end"] = Value::String("true".to_string());
}

type WatchStreamEvent = (crate::cacher::store::WatchEvent, bool);
type WatchEventStream = Pin<Box<dyn tokio_stream::Stream<Item = WatchStreamEvent> + Send + Sync>>;
type WatchFrameFuture = Pin<
    Box<
        dyn Future<Output = Option<Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>>>
            + Send,
    >,
>;

include!("watch_stream.rs");
