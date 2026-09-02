
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
    include!("body-37-1.rs");
    include!("body-37-2.rs");
}
