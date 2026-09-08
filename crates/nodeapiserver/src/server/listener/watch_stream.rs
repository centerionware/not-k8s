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
/// own `client-go` Reflector relists. A bookmark-negotiated connection
/// that produces nothing at all for [`WATCH_IDLE_SILENCE_LIMIT`] is
/// likewise recovered server-side — but at the *connection* level, by the
/// watchdog in `watch_idle`, not by anything in this body (see that
/// module's doc comment for why an in-body timer cannot detect a dead
/// feed). This body's only jobs are to bump the watchdog's frame tracker
/// on every frame it produces and to stop the watchdog when it ends.
/// `StreamBody`/`Frame` come from
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
    idle: Option<WatchIdleGuard>,
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
        idle,
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
    idle: Option<WatchIdleGuard>,
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
    // The one silent way the live half of a watch body can end without
    // this function returning: `Lagged` (the watcher fell behind the
    // cache's bounded broadcast) or `Closed` (the cache was dropped or
    // replaced). That ends the stream and the client relists, but it must
    // show up in the journals — a silently-terminated stream would
    // otherwise be indistinguishable from a healthy-but-quiet watch, which
    // is exactly the ambiguity the idle watchdog's diagnostics (see
    // `watch_idle`) exist to resolve.
    let live_stream = {
        let live_diag = format!("{group}/{version}/{resource}");
        BroadcastStream::new(rx)
            .map_while(move |res| match res {
                Ok(event) => Some(event),
                Err(e) => {
                    tracing::debug!(target: "nk_watch_trace", boundary = "live_stream_ended", resource = %live_diag, error = ?e, "watch: live event stream ended; client will relist");
                    None
                }
            })
            .map(|event| (event, false))
    };
    let events = initial_stream
        .chain(replay_stream)
        .chain(live_stream)
        .filter(move |(event, initial_events_end)| {
            allow_watch_bookmarks
                || *initial_events_end
                || event.kind != crate::cacher::store::EventKind::Bookmark
        });
    // The `timeoutSeconds` bound. A bookmark-negotiated watch (which always
    // carries a connection-level watchdog, `idle`) gets the bound from the
    // watchdog instead of this in-body `take_until`: ending the stream here
    // depends on hyper re-polling a body that a quiet feed leaves parked,
    // so the final EOF is not reliably delivered — observed live as a watch
    // that ended server-side on schedule while the client hung on an EOF
    // that never arrived until its own idle timeout. The watchdog fires the
    // per-connection kill switch at the deadline, which the client observes
    // as a connection close and recovers from with the same relist, and
    // which is driven by tokio's timer regardless of what hyper is doing
    // with the body. Watches that never negotiated bookmarks keep the
    // in-body end (there is no watchdog to own the bound for them).
    let events: WatchEventStream = if let Some(timeout) = timeout {
        if idle.is_none() {
            Box::pin(futures::StreamExt::take_until(
                events,
                tokio::time::sleep(timeout),
            ))
        } else {
            Box::pin(events)
        }
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
        let body = StreamBody::new(frames).boxed();
        return wrap_idle_tracking(body, idle);
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
    wrap_idle_tracking(StreamBody::new(stream).boxed(), idle)
}

/// Wraps a watch response body so it participates in the connection-level
/// idle bound (`watch_idle`): every frame it produces bumps the watchdog's
/// tracker, and when the body ends — or is dropped, which is how hyper
/// tears down a response whose stream never completes — it fires the stop
/// signal so the watchdog exits without killing the connection for a watch
/// that is already gone.
fn wrap_idle_tracking(body: BoxedBody, idle: Option<WatchIdleGuard>) -> BoxedBody {
    use http_body_util::BodyExt as _;
    match idle {
        Some(guard) => IdleWatchBody {
            inner: body,
            tracker: guard.tracker(),
            stop_tx: guard.stop_tx(),
        }
        .boxed(),
        None => body,
    }
}

struct IdleWatchBody {
    inner: BoxedBody,
    tracker: WatchIdleTracker,
    stop_tx: tokio::sync::watch::Sender<bool>,
}

impl http_body::Body for IdleWatchBody {
    type Data = hyper::body::Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        // Every poll is counted, frame or not: the watchdog's diagnostic
        // reads `poll_count` vs `frame_count` to tell "hyper stopped
        // polling this body" from "the body was polled but the event
        // subscription yielded nothing" (see `watch_idle`'s module doc).
        self.tracker.note_poll();
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                self.tracker.note_frame();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                // The encode failed: hyper aborts the response, so this
                // body is done — stop the watchdog like any other end.
                self.tracker.note_poll_outcome(POLL_OUTCOME_ENDED);
                let _ = self.stop_tx.send(true);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.tracker.note_poll_outcome(POLL_OUTCOME_ENDED);
                let _ = self.stop_tx.send(true);
                Poll::Ready(None)
            }
            Poll::Pending => {
                self.tracker.note_poll_outcome(POLL_OUTCOME_PENDING);
                Poll::Pending
            }
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for IdleWatchBody {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
    }
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
