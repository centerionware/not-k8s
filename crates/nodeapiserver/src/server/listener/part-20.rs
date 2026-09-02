
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
