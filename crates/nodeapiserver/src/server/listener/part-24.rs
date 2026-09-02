
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
    include!("body-36-1.rs");
    include!("body-36-2.rs");
}
