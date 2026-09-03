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
        let mut state = self
            .state
            .lock()
            .expect("conversion watch state lock poisoned");
        loop {
            if state.pending.is_some() {
                let poll = state
                    .pending
                    .as_mut()
                    .expect("pending conversion future exists")
                    .as_mut()
                    .poll(cx);
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

            let (event, initial_events_end) = match state.events.as_mut().poll_next(cx) {
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
                    initial_events_end,
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
    watch_response_body_with_initial_events(
        replay,
        rx,
        kind,
        api_version,
        label_reqs,
        field_reqs,
        storage,
        group,
        resource,
        version,
        partial_metadata,
        allow_watch_bookmarks,
        timeout,
        conversion_webhook,
        None,
    )
}

fn watch_response_body_with_initial_events(
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
    initial_events: Option<(Vec<crate::cacher::store::WatchEvent>, i64)>,
) -> BoxedBody {
    use http_body_util::{BodyExt, StreamBody};
    use tokio_stream::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;

    let initial_stream: WatchEventStream = match initial_events {
        Some((initial_events, revision)) => {
            let end = crate::cacher::store::WatchEvent {
                kind: crate::cacher::store::EventKind::Bookmark,
                key: Vec::new(),
                value: Vec::new(),
                revision,
            };
            Box::pin(tokio_stream::iter(
                initial_events
                    .into_iter()
                    .map(|event| (event, false))
                    .chain(std::iter::once((end, true))),
            ))
        }
        None => Box::pin(tokio_stream::empty()),
    };
    let replay_stream = tokio_stream::iter(replay).map(|event| (event, false));
    let live_stream = BroadcastStream::new(rx)
        .map_while(|res| res.ok())
        .map(|event| (event, false));
    let events = initial_stream
        .chain(replay_stream)
        .chain(live_stream)
        .filter(move |(event, initial_events_end)| {
            allow_watch_bookmarks
                || *initial_events_end
                || event.kind != crate::cacher::store::EventKind::Bookmark
        });
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
    let (storage_for_filter, group_for_filter, resource_for_filter) =
        (storage.clone(), group.clone(), resource.clone());
    let filtered = events.filter(move |(event, _)| {
        watch_event_matches_selector(
            event,
            &label_reqs,
            &field_reqs,
            storage_for_filter.as_ref(),
            &group_for_filter,
            &resource_for_filter,
        )
    });
    if conversion_webhook.is_none() {
        let frames = filtered.filter_map(move |(event, initial_events_end)| {
            encode_watch_event(
                &event,
                &kind,
                &api_version,
                storage.as_ref(),
                &group,
                &resource,
                &version,
                partial_metadata,
                initial_events_end,
            )
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

/// Return the upstream-compatible response for a watch that started below
/// the cache's retained history. The HTTP request has already passed the
/// normal watch admission/authentication path, so the expiration is a watch
/// event (`type: ERROR`, `Status.code: 410`) inside an HTTP 200 response,
/// not an HTTP-level error. Clients such as kube-rs reset their watcher and
/// relist when they receive that in-band event.
fn watch_resource_expired_response(path: &str) -> Response<BoxedBody> {
    let event = serde_json::json!({
        "type": "ERROR",
        "object": resource_expired_status(path),
    });
    let mut bytes = serde_json::to_vec(&event).unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(body_from_bytes(bytes))
        .unwrap()
}
