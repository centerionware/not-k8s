//! The REST/watch listener: binds `cfg.bind_addr` over TLS, accepts
//! connections, and serves them with hyper's auto (h1/h2) connection
//! builder — h2 matters here in a way it didn't for nodelet's own HTTPS
//! server (`CLAUDE.md`: client-go and kubectl negotiate h2, and watch
//! multiplexing depends on it). Structure mirrors
//! `crates/nodelet/src/server/mod.rs`'s own `run()` closely — same
//! accept-loop/TLS-handshake/spawn-per-connection shape, adapted for a
//! listener with no reason to ever be individually disabled the way
//! nodelet's exec/logs server is.
//!
//! **`GET`, `LIST`, `CREATE`, single-object `DELETE`, and `UPDATE`
//! against a real resource are now real** (`rest::get`/`rest::list`/
//! `rest::create`/`rest::delete`/`rest::update`, generic over every
//! resource this build knows about — see `rest`'s own doc comment for
//! exactly what's in and out of scope). `run()` also spawns a real
//! `cacher::registry::CacheRegistry` cache for `BOOT_CACHED_RESOURCES` —
//! a deliberately bounded, reasoned list (mostly core-group, plus real
//! `crates/nodeproxy`'s own actual `discovery.k8s.io/v1` `EndpointSlice`
//! dependency) of the resources a real cluster's own kubelets/kube-proxy/
//! controllers read most heavily, not every resource this build knows
//! about — and `GET`/`LIST`
//! consult one whenever the request targets a resource in that list
//! (`rest::get`/`rest::list`'s own `Option<&SharedCache>` parameter);
//! every other resource still reads straight from nodestore (see
//! `cacher::registry`'s own doc comment for why enumerating *every*
//! resource at boot still isn't done). `WATCH` against a resource with a
//! registered cache is real too now (`is_watch`'s own doc comment, and
//! `watch_response_body`): the cache's own retained history replays
//! first, then live events stream as they happen, real `Transfer-Encoding`
//! framing handled by hyper's own h1/h2 connection layer. A resource with
//! no registered cache still falls through to the echo stub, same as a
//! resource `GET`/`LIST` can't yet serve from a cache. `PATCH` is real
//! too now (`rest::patch_prepare`/`patch_persist`, reusing Group G's
//! already-landed `patch::json_patch`/`merge_patch`/`strategic_merge`,
//! selected by the real `Content-Type` —
//! `application/json-patch+json`/`application/merge-patch+json`/
//! `application/strategic-merge-patch+json` — with a real `415` for
//! anything else), **and now runs the two Group J plugins that ever
//! apply to an `Update`-shaped write** (`namespace_lifecycle`,
//! `LimitRanger`'s own PVC validation — the split between
//! `rest::patch_prepare`/`patch_persist` exists specifically so admission
//! can see the real candidate object in between the two). `deletecollection`
//! is real too now (`rest::delete_collection` — lists via the same
//! selector filtering `LIST` already has, then deletes each match) —
//! **it alone still runs no Group J admission**, a named gap (in
//! practice a small one: `namespace_lifecycle`'s own immortal-namespace
//! check needs a `name`, which a collection delete never has, so the
//! only real loss is `LimitRanger`'s own PVC check, which a bulk PVC
//! delete wouldn't be blocked by anyway — deleting under a limit never
//! violates a *minimum*). `watch` is the only remaining resource verb
//! this build knows about that isn't a real generic REST dispatch — it's
//! real too, just structurally different (a streaming response, covered
//! above).
//! Client certificate authentication is real (`super::tls`'s optional
//! `client_ca`, `authn::x509::identity_from_der` on the verified peer
//! cert), surfaced in the echo response's own `user` field for
//! observability. Authorization is real too, but **opt-in and off by
//! default** (`config::Config::enforce_rbac`/`NODEAPISERVER_ENFORCE_RBAC`
//! — see that field's own doc comment for why: enabling RBAC enforcement
//! before Group O's bootstrap `ClusterRole`/`ClusterRoleBinding` set
//! exists can lock every request out with no path back in). When
//! enabled, `GET`/`LIST`/`CREATE`/`DELETE`/`UPDATE`/`WATCH` all resolve
//! the caller's real rules (`authz::resolve::rules_for` — the real
//! anonymous identity, `system:anonymous`/`system:unauthenticated`, when
//! no x509 identity was established) and deny with a real `403` unless
//! `authz::rbac::rules_allow` says yes. Group J admission (five
//! unconditional plugins as of this revision, `admission`'s own doc
//! comment has the running list) also runs, on the real write verbs only
//! — real upstream's own admission posture too, admission never gates a
//! read. The real handler chain (authentication -> authorization ->
//! priority-and-fairness -> admission -> REST, `docs/APISERVER.md`'s own
//! hard requirement) is still not fully unified into one ordered
//! pipeline — each piece above is wired in ad hoc, in the right relative
//! order for what exists today, not through one shared dispatcher yet.
//!
//! What *is* real now: `/healthz`/`/readyz`/`/livez` (`server::healthz` —
//! real upstream's own per-check response shape, `?verbose` included; see
//! that module's own doc comment for exactly which checks are ported),
//! and every non-resource discovery route
//! (`/api`, `/api/{version}`, `/apis`, `/apis/{group}`,
//! `/apis/{group}/{version}`, `/openapi/v3(/...)`, `/version`) is answered
//! by `server::discovery`/`openapi`/`version`'s real document builders
//! (`route_discovery`, pure and unit-tested below) rather than falling
//! into the generic echo — these are the routes `kubectl` itself calls
//! first (RESTMapper discovery) before it can even shape a request for an
//! actual resource, so wiring them is what makes the listener minimally
//! useful to a real client rather than just a path-grammar demo. `/api`
//! and `/apis` also negotiate: a client asking for aggregated discovery
//! v2 (`as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io`, real
//! client-go's own aggregated discovery client) gets that shape instead of
//! the legacy `APIVersions`/`APIGroupList`, via `codec::negotiation`.
//! A discovery-shaped path this build doesn't serve (unknown
//! group/version) gets a real `404` with a minimal `Status` body — not
//! yet upstream's full `Status` type (`reason`/`details` machinery is
//! real hand-written work for a later group), but shaped close enough
//! that `client-go`'s own error-decoding path reads `code`/`reason`/
//! `message` off exactly this JSON today.

use crate::admission;
use crate::aggregator;
use crate::authz;
use crate::flowcontrol;
use crate::proxy;
use crate::config::Config;
use crate::codec::negotiation;
use crate::server::{discovery, healthz, metrics, openapi, path, rest, version};
use crate::storage::client::StorageClient;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BoxedBody = http_body_util::combinators::BoxBody<hyper::body::Bytes, BoxError>;

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
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder().status(status).header("Content-Type", "application/json").body(body_from_bytes(bytes)).unwrap()
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
    path::parse_query(query).into_iter().find(|(k, _)| k == "fieldManager").map(|(_, v)| v)
}

/// Real upstream's own `?force=` query parameter — Server-Side Apply's
/// conflict-override flag.
fn force_query(query: &str) -> bool {
    path::parse_query(query).iter().any(|(k, v)| k == "force" && v == "true")
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
fn encode_watch_event(event: &crate::cacher::store::WatchEvent, kind: &str, api_version: &str, storage: Option<&StorageClient>, group: &str, resource: &str, version: &str) -> Option<Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>> {
    match crate::server::watch_event::to_watch_event_json(event, kind, api_version, storage, group, resource) {
        None => None,
        Some(Ok(json)) => {
            let mut bytes = serde_json::to_vec(&json).unwrap_or_default();
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
) -> BoxedBody {
    use http_body_util::{BodyExt, StreamBody};
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt;

    let replay_stream = tokio_stream::iter(replay);
    let live_stream = BroadcastStream::new(rx).map_while(|res| res.ok());
    let events = replay_stream.chain(live_stream);
    // Cloned once per closure (`StorageClient` wraps a cheap-to-clone
    // `tonic::transport::Channel`, same posture every other real call
    // site in this crate already takes) — `filter`/`filter_map` each need
    // their own `'static`-owned copy of the encryption-lookup context.
    let (storage_for_filter, group_for_filter, resource_for_filter) = (storage.clone(), group.clone(), resource.clone());
    let filtered = events.filter(move |event| watch_event_matches_selector(event, &label_reqs, &field_reqs, storage_for_filter.as_ref(), &group_for_filter, &resource_for_filter));
    let frames = filtered.filter_map(move |event| encode_watch_event(&event, &kind, &api_version, storage.as_ref(), &group, &resource, &version));
    StreamBody::new(frames).boxed()
}

/// Runs the listener forever (until the process exits). Best-effort on
/// bind/TLS failure — logs and returns rather than panicking, matching
/// every other background loop's degrade-and-continue posture in this
/// workspace (see `crates/nodelet/src/server/mod.rs::run`'s own doc
/// comment for the precedent).
pub async fn run(cfg: Config) {
    let cert_result = match (&cfg.tls_cert_file, &cfg.tls_key_file) {
        (Some(cert), Some(key)) => super::tls::load_from_pem(cert, key),
        _ => {
            let cert_dir = std::path::PathBuf::from("/var/lib/nodeapiserver/pki");
            let sans = vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "kubernetes".to_string(),
                "kubernetes.default".to_string(),
            ];
            super::tls::load_or_generate(&cert_dir, &sans)
        }
    };
    let cert = match cert_result {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "failed to load/generate the TLS certificate; the REST/watch listener will not run");
            return;
        }
    };

    // Group H, first slice: client certificate authentication, offered
    // but not required (see server::tls's own doc comment). Best-effort
    // like everything else here — a misconfigured/unreadable CA file
    // disables client-cert auth for this run rather than stopping the
    // listener, since `client_ca_file` being set at all is optional in
    // the first place.
    let client_ca = match &cfg.client_ca_file {
        Some(path) => match super::tls::load_client_ca(path) {
            Ok(store) => Some(store),
            Err(e) => {
                warn!(path = %path.display(), error = ?e, "failed to load NODEAPISERVER_CLIENT_CA_FILE; client certificate authentication is disabled for this run");
                None
            }
        },
        None => None,
    };

    // Group H: ServiceAccount JWTs are optional for standalone development,
    // but the nodebootstrap target supplies the cluster signing key so
    // projected pod tokens and nodelet's TokenReview fallback work before
    // RBAC enforcement is enabled.
    let service_account_authenticator = match &cfg.service_account_signing_key_file {
        Some(path) => match crate::authn::service_account::Authenticator::from_pem(path, cfg.service_account_issuer.clone()) {
            Ok(authenticator) => Some(Arc::new(authenticator)),
            Err(e) => {
                warn!(path = %path.display(), error = ?e, "failed to load NODEAPISERVER_SERVICE_ACCOUNT_SIGNING_KEY_FILE; the REST/watch listener will not run");
                return;
            }
        },
        None => None,
    };

    let server_config = match cert.server_config(client_ca.as_ref()) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "failed to build TLS server config; the REST/watch listener will not run");
            return;
        }
    };
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    // Group C: load and validate `EncryptionConfiguration` *before*
    // connecting to nodestore — a misconfigured file is a real, loud
    // startup failure this way, and the parsed config needs to be ready
    // to attach to `storage` the moment it exists, before any clone of
    // it (the cache-registry spawn loop below, or a per-connection clone
    // in the accept loop) gets made without it.
    let encryption_config = match &cfg.encryption_config_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(yaml) => match crate::storage::encryption_config::parse(&yaml) {
                Ok(parsed) => {
                    info!(path = %path.display(), entries = parsed.entries.len(), "nodeapiserver: loaded EncryptionConfiguration");
                    Some(parsed)
                }
                Err(e) => {
                    warn!(path = %path.display(), error = ?e, "invalid NODEAPISERVER_ENCRYPTION_CONFIG_FILE; continuing with no encryption-at-rest");
                    None
                }
            },
            Err(e) => {
                warn!(path = %path.display(), error = ?e, "failed to read NODEAPISERVER_ENCRYPTION_CONFIG_FILE; continuing with no encryption-at-rest");
                None
            }
        },
        None => None,
    };

    // Best-effort, matching every other failure in this function: a
    // nodestore that isn't reachable yet at startup shouldn't stop the
    // listener from serving discovery (which needs no storage at all) —
    // `rest::get` degrades to the bring-up echo stub when this is `None`
    // (see its own call site's comment). Connected once here and cloned
    // per connection below: `StorageClient` wraps a cheap-to-clone
    // `tonic::transport::Channel`, the same "clone per use, don't share a
    // `&mut` behind a lock" posture `cacher`'s own driver takes.
    // `with_encryption` attaches Group C's config to `storage` right
    // away — before `cache_registry.spawn` below ever clones it — so
    // every clone made from this point on (including every long-running
    // background reflect loop) carries it too.
    let storage = match StorageClient::connect(&cfg).await {
        Ok(c) => Some(c.with_encryption(encryption_config)),
        Err(e) => {
            warn!(error = ?e, "failed to connect to nodestore at startup; resource GET requests will fall back to the bring-up echo stub until this succeeds");
            None
        }
    };

    // Group D: a real, deliberately bounded first expansion beyond the
    // original one-resource (`namespaces`) proof of concept. Registering
    // a cache for every resource this build knows about at boot is still
    // a real, separate, not-yet-made policy decision (`cacher::registry`'s
    // own doc comment: spawning on the order of 90 concurrent,
    // long-running reconnect loops at startup needs an ordering/pacing
    // decision this crate hasn't made) — `BOOT_CACHED_RESOURCES` is a
    // reasoned, small subset instead: the resources a real cluster's own
    // kubelets/kube-proxy/controllers read most heavily (GET/LIST-heavy,
    // write-light) -- mostly core-group, plus real `crates/nodeproxy`'s
    // own actual `discovery.k8s.io/v1` `EndpointSlice` dependency, not
    // an attempt at the general policy. `StorageClient::clone()` is
    // cheap (a `tonic::transport::Channel`
    // clone), so registering several of these costs no extra real
    // connections.
    let cache_registry = crate::cacher::CacheRegistry::new();
    if let Some(s) = storage.as_ref() {
        for (group, version, resource) in BOOT_CACHED_RESOURCES {
            cache_registry.spawn(s.clone(), group, version, resource);
        }
    }

    // Group L Phase 2: the live `APIService` availability reconciliation
    // loop (`aggregator::reconcile`'s own doc comment covers the real
    // scope) — best effort, same posture the cache-registry spawn loop
    // just above already has: no storage at startup just means this
    // loop never runs, not a reason to stop the listener. A fixed
    // interval, not watch-driven (`aggregator::reconcile`'s own real
    // work — a Service/EndpointSlice health check, a live network dial —
    // is exactly the kind of externally-changing state real upstream's
    // own controller resyncs periodically for too, not purely reactive
    // to `APIService` object mutations).
    if let Some(s) = storage.as_ref() {
        let mut reconcile_storage = s.clone();
        tokio::spawn(async move {
            loop {
                match crate::aggregator::reconcile::reconcile_once(&mut reconcile_storage).await {
                    Ok(n) if n > 0 => info!(reconciled = n, "aggregator: reconciled APIService availability"),
                    Ok(_) => {}
                    Err(e) => warn!(error = ?e, "aggregator: APIService availability reconciliation pass failed"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

    let addr: SocketAddr = match cfg.bind_addr.parse() {
        Ok(a) => a,
        Err(e) => {
            warn!(bind_addr = %cfg.bind_addr, error = ?e, "invalid NODEAPISERVER_BIND_ADDR");
            return;
        }
    };
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!(%addr, error = ?e, "failed to bind the REST/watch listener port");
            return;
        }
    };
    info!(%addr, storage_connected = storage.is_some(), enforce_rbac = cfg.enforce_rbac, cached_resources = BOOT_CACHED_RESOURCES.len(), "nodeapiserver: REST/watch listener up (discovery + GET/LIST/CREATE/DELETE/UPDATE are real; every other resource verb is still a bring-up stub — see server::listener's own doc comment)");
    let enforce_rbac = cfg.enforce_rbac;

    // Group N: built once at startup, not per request — the TLS config
    // itself doesn't depend on which pod/node a given `pods/log` request
    // targets, only on this crate's own static configuration
    // (`NODEAPISERVER_KUBELET_CLIENT_CERT_FILE`/`_KEY_FILE`). Best-effort
    // like everything else here: a misconfigured cert/key pair falls
    // back to no client identity (the same "connects, but nodelet's own
    // TokenReview fallback path has nothing to accept" situation an
    // unset config already produces), logged rather than stopping the
    // listener.
    let kubelet_client_cert_key = match (&cfg.kubelet_client_cert_file, &cfg.kubelet_client_key_file) {
        (Some(cert), Some(key)) => Some((cert.as_path(), key.as_path())),
        _ => None,
    };
    let kubelet_tls = std::sync::Arc::new(match crate::proxy::client_tls::build_client_config(kubelet_client_cert_key) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "failed to build the kubelet-proxy TLS client config with the configured client cert; falling back to no client identity");
            crate::proxy::client_tls::build_client_config(None).expect("a client config with no client cert must always succeed")
        }
    });

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = ?e, "listener: accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let storage = storage.clone();
        let cache_registry = cache_registry.clone();
        let kubelet_tls = kubelet_tls.clone();
        let service_account_authenticator = service_account_authenticator.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%peer, error = ?e, "listener: TLS handshake failed");
                    return;
                }
            };
            // Group H: if the client presented a certificate and it chains
            // to the configured CA (rustls already verified this during
            // the handshake above — `with_client_cert_verifier`'s job, not
            // this code's), extract its identity. `None` either because no
            // client-cert auth is configured at all, or because this
            // particular client didn't present one — both are the same
            // "unauthenticated by x509" outcome from here.
            let identity = tls_stream.get_ref().1.peer_certificates().and_then(|certs| certs.first()).and_then(|leaf| crate::authn::x509::identity_from_der(leaf.as_ref()));
            let io = TokioIo::new(tls_stream);
            let service = hyper::service::service_fn(move |req| handle_with_audit(req, storage.clone(), cache_registry.clone(), identity.clone(), service_account_authenticator.clone(), enforce_rbac, peer, kubelet_tls.clone()));
            if let Err(e) = ConnBuilder::new(TokioExecutor::new()).serve_connection_with_upgrades(io, service).await {
                tracing::debug!(%peer, error = ?e, "listener: connection ended");
            }
        });
    }
}

/// Outcome of trying to route a path as one of the five non-resource
/// discovery endpoints. Kept distinct from a plain `Option<Value>` so the
/// caller can tell "not a discovery-shaped path at all, fall through to
/// resource handling" apart from "was discovery-shaped, but this build
/// serves no such group/version" — the latter is a real `404`, not a
/// silent fallthrough into the resource-request echo stub, which would
/// otherwise mis-describe a `/apis/totally.made.up/v1` request as some
/// kind of resource request.
enum DiscoveryRoute {
    NotApplicable,
    Found(serde_json::Value),
    /// Same as `Found`, but the bytes are already-serialized JSON (an
    /// `/openapi/v3/<path>` document, embedded verbatim at build time) —
    /// serving them directly avoids a pointless parse-then-reserialize
    /// round trip through `serde_json::Value` for a payload that can be
    /// tens of kilobytes.
    FoundRaw(&'static [u8]),
    NotFound,
}

/// `true` if `accept_header` asks for aggregated discovery v2
/// (`as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io`) via
/// `codec::negotiation` — the same header real client-go's aggregated
/// discovery client sends when it wants one `/api`/`/apis` call instead of
/// the legacy `/apis` + one `/apis/{group}/{version}` per group-version.
/// Requires an exact `v2` match (not `v2beta1`, the pre-GA shape this
/// crate doesn't separately model) rather than accepting any version
/// under that group, so a client asking for a shape this build doesn't
/// actually build never silently gets served a possibly-wrong one.
fn wants_aggregated_discovery(accept_header: Option<&str>) -> bool {
    let Some(header) = accept_header else { return false };
    let Some(accepted) = negotiation::negotiate(header) else { return false };
    accepted.as_kind.as_deref() == Some("APIGroupDiscoveryList") && accepted.as_group.as_deref() == Some("apidiscovery.k8s.io") && accepted.as_version.as_deref() == Some("v2")
}

/// Pure and unit-tested (unlike `handle`, which needs a live TLS
/// connection to exercise at all): `parts` is the already-split, prefix-
/// intact path (`["api", "v1"]`, `["apis", "apps", "v1"]`, ...) from
/// [`path::split_path`]. `accept_header` is the raw `Accept` header value,
/// if any — its only job here is picking legacy vs. aggregated discovery
/// for the two group-list routes (`/api`, `/apis`); every other route
/// ignores it entirely (it already only serves one shape).
/// `crds` — Group K's own discovery merge: every served, `Established`
/// CRD's resources, only ever non-empty for an `/apis`-prefixed path
/// (the core group at `/api` never has CRDs in it — a CRD's own
/// `spec.group` is never empty, real upstream's own CRD validation
/// requires it). `handle`'s own call site fetches this live (one `LIST`
/// of `customresourcedefinitions`) only when the path actually starts
/// with `apis`, rather than paying that cost on every single discovery
/// request — see that call site's own comment.
/// The pure decision half of Group L Phase 3's live discovery proxy: is
/// `parts` exactly a bare `/apis/{group}/{version}` path (`route_discovery`'s
/// own `NotFound` outcome for it means no local answer exists at all —
/// not statically, not via a CRD), and does `aggregated` (the same
/// pre-flight-gated live list `server::listener::handle`'s own caller
/// already fetched) claim that exact `(group, version)`? `Some` hands
/// back borrowed references into `parts`/`aggregated` themselves — no
/// cloning needed, the caller only ever uses them for one more `resolve`
/// call before either succeeding or falling through to a real `404`.
fn aggregated_discovery_group_version<'a>(parts: &'a [String], aggregated: &'a [(String, String)]) -> Option<(&'a str, &'a str)> {
    if parts.len() != 3 || parts[0] != "apis" {
        return None;
    }
    aggregated.iter().find(|(g, v)| g == &parts[1] && v == &parts[2]).map(|(g, v)| (g.as_str(), v.as_str()))
}

fn route_discovery(parts: &[String], accept_header: Option<&str>, crds: &[crate::apiextensions::registry::DiscoverableResource], aggregated: &[(String, String)]) -> DiscoveryRoute {
    let seg = |i: usize| parts.get(i).map(String::as_str);
    match (seg(0), seg(1), parts.len()) {
        (Some("api"), _, 1) if wants_aggregated_discovery(accept_header) => DiscoveryRoute::Found(discovery::api_v1_group_discovery_list_with_crds()),
        (Some("api"), _, 1) => DiscoveryRoute::Found(discovery::api_versions()),
        (Some("api"), _, 2) => match discovery::api_resource_list("", &parts[1]) {
            Some(doc) => DiscoveryRoute::Found(doc),
            None => DiscoveryRoute::NotFound,
        },
        (Some("apis"), _, 1) if wants_aggregated_discovery(accept_header) => DiscoveryRoute::Found(discovery::api_group_discovery_list_with_crds(crds, aggregated)),
        (Some("apis"), _, 1) => DiscoveryRoute::Found(discovery::api_group_list_with_crds(crds, aggregated)),
        (Some("apis"), _, 2) => match discovery::api_group_with_crds(&parts[1], crds, aggregated) {
            Some(doc) => DiscoveryRoute::Found(doc),
            None => DiscoveryRoute::NotFound,
        },
        (Some("apis"), _, 3) => match discovery::api_resource_list_with_crds(&parts[1], &parts[2], crds) {
            Some(doc) => DiscoveryRoute::Found(doc),
            None => DiscoveryRoute::NotFound,
        },
        (Some("openapi"), Some("v3"), 2) => DiscoveryRoute::Found(openapi::root()),
        (Some("openapi"), Some("v3"), n) if n > 2 => match openapi::doc(&parts[2..].join("/")) {
            Some(bytes) => DiscoveryRoute::FoundRaw(bytes),
            None => DiscoveryRoute::NotFound,
        },
        (Some("version"), _, 1) => DiscoveryRoute::Found(version::info()),
        _ => DiscoveryRoute::NotApplicable,
    }
}

/// A minimal `meta/v1.Status` body for a `404` — real upstream's full
/// `Status` type (structured `details.causes`, per-reason `retryAfter`,
/// ...) isn't built yet (Group E/J territory), but `kind`/`apiVersion`/
/// `status`/`message`/`reason`/`code` is exactly what `client-go`'s own
/// `errors.NewNotFound`-decoding path (`apimachinery/pkg/api/errors`)
/// reads off an error response, so this shape is a real, not approximate,
/// subset rather than an invented one.
fn not_found_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("the server could not find the requested resource ({path_str})"),
        "reason": "NotFound",
        "details": {},
        "code": 404,
    })
}

/// Same minimal `Status` shape as [`not_found_status`], for the one real
/// failure mode `rest::get` can hit that isn't "not found" — a nodestore
/// request that itself errored (connection drop, decode failure on
/// malformed stored data, ...).
fn internal_error_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("the server encountered an internal error handling {path_str}"),
        "reason": "InternalError",
        "details": {},
        "code": 500,
    })
}

fn unauthorized_status(path_str: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: {detail}"),
        "reason": "Unauthorized",
        "details": {},
        "code": 401,
    })
}

/// Same minimal `Status` shape again, for a request the client itself
/// malformed (today: an unparsable `labelSelector`/`fieldSelector`) —
/// real upstream's `reason: "BadRequest"`, `code: 400`.
fn bad_request_status(path_str: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: {detail}"),
        "reason": "BadRequest",
        "details": {},
        "code": 400,
    })
}

/// Same minimal `Status` shape again, for an RBAC denial (`enforce_rbac`
/// only — see this module's own doc comment) — real upstream's
/// `reason: "Forbidden"`, `code: 403`.
fn forbidden_status(path_str: &str, user_name: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: User {user_name:?} does not have permission for this request (RBAC)"),
        "reason": "Forbidden",
        "details": {},
        "code": 403,
    })
}

/// Same minimal `Status` shape, for a Group J admission denial (today:
/// only `admission::namespace_lifecycle`) — real upstream's `reason:
/// "Forbidden"`, `code: 403`, same as an RBAC denial's shape but carrying
/// the plugin's own message rather than a generic "does not have
/// permission" one.
fn admission_forbidden_status(path_str: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: {detail}"),
        "reason": "Forbidden",
        "details": {},
        "code": 403,
    })
}

/// Real upstream's own shape for a proxy subresource (`pods/log`, ...)
/// whose dial to the real backend (nodelet) itself failed — `reason:
/// "" ` (upstream doesn't set one for this case either), `code: 502`,
/// distinct from [`internal_error_status`]'s `500` because the fault is
/// nodelet/the network, not this process.
fn bad_gateway_status(path_str: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: {detail}"),
        "reason": "",
        "details": {},
        "code": 502,
    })
}

/// Real upstream's own `ServiceUnavailable` shape — used here when an
/// aggregated `APIService`'s own pre-flight check
/// (`aggregator::availability::preflight_check`) fails: the backing
/// Service/EndpointSlice state itself is the fault, not this process nor
/// the backend's own dial (that's [`bad_gateway_status`]'s case
/// instead), matching real upstream's own `errors.NewServiceUnavailable`
/// for the identical real situation.
fn service_unavailable_status(path_str: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: {detail}"),
        "reason": "ServiceUnavailable",
        "details": {},
        "code": 503,
    })
}

/// Real upstream's own `AlreadyExists` shape for a `CREATE` that lost the
/// create-only-if-absent race — `reason: "AlreadyExists"`, `code: 409`.
fn conflict_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: object already exists"),
        "reason": "AlreadyExists",
        "details": {},
        "code": 409,
    })
}

/// Real upstream's own `Invalid` shape for a `CREATE` that failed
/// `scheme::validation` — `reason: "Invalid"`, `code: 422`. Real
/// upstream's full `Status.details.causes` (one structured entry per
/// violation) isn't built — `message` joins every violation into one
/// human-readable string instead, same "real subset, not the full type"
/// posture every other `Status` builder in this module already takes.
fn invalid_status(path_str: &str, violations: &[String]) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str} is invalid: {}", violations.join("; ")),
        "reason": "Invalid",
        "details": {},
        "code": 422,
    })
}

/// Real upstream's own `user.Anonymous`/`user.AllUnauthenticated`
/// constants — what a request with no established identity is treated
/// as for authorization purposes (RBAC then denies it unless some policy
/// explicitly grants access to `system:anonymous`/`system:unauthenticated`,
/// same as real upstream).
const ANONYMOUS_USERNAME: &str = "system:anonymous";
const UNAUTHENTICATED_GROUP: &str = "system:unauthenticated";

/// `(group, version, resource)` — `run()`'s own deliberately bounded
/// first expansion of Group D's cache registration beyond the original
/// single-resource (`namespaces`) proof of concept. See the call site's
/// own doc comment for why this list, not "every resource," is the
/// reasoned choice today.
const BOOT_CACHED_RESOURCES: &[(&str, &str, &str)] = &[
    ("", "v1", "namespaces"),
    ("", "v1", "pods"),
    ("", "v1", "services"),
    ("", "v1", "secrets"),
    ("", "v1", "configmaps"),
    ("", "v1", "endpoints"),
    ("", "v1", "nodes"),
    // Real `crates/nodeproxy` watches `EndpointSlice`, not the legacy
    // core/v1 `Endpoints` API this list already carried above
    // (`crates/nodeproxy/src/svc.rs`'s own doc comment: "Backends come
    // from `EndpointSlice` (not the legacy `Endpoints` API)") -- the
    // resource this list's own stated rationale ("kube-proxy... read
    // most heavily") actually meant was missing entirely until now.
    ("discovery.k8s.io", "v1", "endpointslices"),
];

/// Group J: persists `ResourceQuota.status.used` after a successful pod/
/// PVC/service `CREATE`, or the generic object-count evaluator's own
/// `count/<resource>` fallback — real upstream's own
/// `quotaAccessor.UpdateQuotaStatus`
/// (`plugin/pkg/admission/resourcequota/apis/resourcequota/...`),
/// scoped to whichever evaluator's own `usage_after_*_create` the caller
/// already computed. A bounded retry (3 attempts) on a real optimistic-
/// concurrency `Conflict` from `rest::update_status` re-reads the quota
/// and merges again, same "retry on lost race" posture every other write
/// path in this crate already uses. **Read-modify-write, not
/// overwrite**: only the keys the calling evaluator itself tracks are
/// replaced in the quota's existing `status.used` map — every
/// `ResourceQuota` evaluator this crate has now persists its own
/// `status.used` this way, so the read-modify-write is what keeps them
/// from clobbering each other's keys, not a "some evaluator doesn't
/// persist yet" gap. Every failure (quota vanished, storage error, retries
/// exhausted) is logged and dropped — a status write is bookkeeping, not the
/// admission decision itself, which has already succeeded by the time
/// this runs.
async fn persist_quota_usage_updates(client: &mut StorageClient, namespace: &str, updates: Vec<(String, std::collections::BTreeMap<String, crate::scheme::quantity::Quantity>)>, path_str: &str) {
    for (quota_name, new_usage) in updates {
        for _attempt in 0..3 {
            let current = match rest::get(client, None, "", "v1", "resourcequotas", Some(namespace), &quota_name).await {
                Ok(rest::GetOutcome::Found(q)) => q,
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => break,
                Err(e) => {
                    warn!(path = %path_str, %quota_name, error = ?e, "admission: reading ResourceQuota to persist status.used failed");
                    break;
                }
            };
            let mut merged: std::collections::BTreeMap<String, crate::scheme::quantity::Quantity> = current
                .pointer("/status/used")
                .and_then(serde_json::Value::as_object)
                .map(|m| m.iter().filter_map(|(k, v)| v.as_str().and_then(|s| crate::scheme::quantity::Quantity::parse(s).ok()).map(|q| (k.clone(), q))).collect())
                .unwrap_or_default();
            for (k, v) in &new_usage {
                merged.insert(k.clone(), *v);
            }
            let mut status_body = current.clone();
            status_body["status"]["used"] = serde_json::Value::Object(merged.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.to_string()))).collect());

            match rest::update_status(client, "", "v1", "resourcequotas", Some(namespace), &quota_name, &status_body).await {
                Ok(rest::UpdateOutcome::Updated(_)) => break,
                Ok(rest::UpdateOutcome::Conflict) => continue,
                Ok(_) => break,
                Err(e) => {
                    warn!(path = %path_str, %quota_name, error = ?e, "admission: persisting ResourceQuota.status.used failed");
                    break;
                }
            }
        }
    }
}

/// Group M: wraps every request with a real `audit::event::build_event`
/// call, logged rather than delegated back into `handle` itself — this
/// wrapper needs nothing `handle` doesn't already compute internally
/// (method/path/query are read off `req` before it's ever consumed, and
/// `path::parse` is a pure function safe to call a second time here),
/// so it's the far less invasive place to add auditing than threading an
/// audit-context return value out through every one of `handle`'s own
/// early-return branches would have been. **The sink is this crate's own
/// `tracing` output** (`target: "nodeapiserver::audit"`, one JSON line
/// per request) — a real, working choice consistent with how every other
/// component in this workspace already does its own logging (no
/// component here writes to a separate log file), not real upstream's
/// own dedicated `--audit-log-path` file with rotation, and not a
/// webhook backend either; an operator wanting a separate audit stream
/// filters this crate's own log output by that target today. See
/// `audit::event`'s own doc comment for exactly which real `Event`
/// fields are populated and which stage/level this always uses.
async fn handle_with_audit(
    req: Request<Incoming>,
    storage: Option<StorageClient>,
    cache_registry: crate::cacher::CacheRegistry,
    identity: Option<crate::authn::x509::Identity>,
    service_account_authenticator: Option<Arc<crate::authn::service_account::Authenticator>>,
    enforce_rbac: bool,
    peer: SocketAddr,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Result<Response<BoxedBody>, Infallible> {
    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let user_agent = req.headers().get("user-agent").and_then(|v| v.to_str().ok()).map(str::to_string);
    let identity = match authenticate_request(&req, identity, service_account_authenticator.as_deref()) {
        Ok(identity) => identity,
        Err(detail) => return Ok(json_response(StatusCode::UNAUTHORIZED, &unauthorized_status(&path_str, detail))),
    };
    let audit_identity = identity.clone();
    // Group M (APF): a cheap clone (wraps a `tonic::transport::Channel`,
    // same reasoning PR #107's watch-RBAC-gating clone already
    // established) so flow-schema resolution below has its own
    // connection independent of whatever `handle()` does with the one it
    // owns.
    let storage_for_pf = storage.clone();

    // Group M: `apiserver_request_duration_seconds`'s own start time —
    // measured around the exact same `handle()` call the audit event and
    // `apiserver_request_total` are both already keyed off of. For
    // `watch` specifically this measures time-to-first-byte (when
    // `handle()` returns the still-streaming response), not the full
    // stream lifetime — the identical, already-named caveat
    // `log_audit_event`'s own `ResponseComplete`-at-stream-start choice
    // has, not a new gap this metric introduces.
    let start = std::time::Instant::now();
    let mut response = handle(req, storage, cache_registry, identity, service_account_authenticator, enforce_rbac, kubelet_tls).await;
    let elapsed = start.elapsed().as_secs_f64();

    if let Ok(resp) = &mut response {
        let status = resp.status().as_u16();
        log_audit_event(&method, &path_str, &query, user_agent.as_deref(), audit_identity.as_ref(), &peer, status);
        // Group M: `/metrics`'s own request counter (`server::metrics`) —
        // recorded from the exact same parsed `RequestInfo` the audit
        // event above already builds, so a non-resource request (a
        // discovery route, `/healthz`, ...) is counted under its real
        // verb with an empty `resource` label, matching real upstream's
        // own convention for that case.
        let info = path::parse(&method, &path_str, &query);
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

        // Group M (APF): label the response with which FlowSchema/
        // PriorityLevelConfiguration would govern it, real upstream's own
        // observable behavior even before this build does any actual
        // queuing/limiting — see `flowcontrol::resolve`'s own doc comment.
        // Only for real resource/non-resource requests handled past
        // discovery (matching real upstream's own filter chain scope);
        // skipped entirely when there's no storage connection to resolve
        // against.
        if let Some(mut client) = storage_for_pf {
            let (user_name, user_groups): (&str, Vec<String>) = match audit_identity.as_ref() {
                Some(id) => (id.name.as_str(), id.groups.clone()),
                None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
            };
            let digest = flowcontrol::flow_schema::RequestDigest {
                user_name,
                user_groups: &user_groups,
                verb: &info.verb,
                is_resource_request: info.is_resource_request,
                api_group: &info.api_group,
                resource: &info.resource,
                subresource: &info.subresource,
                namespace: &info.namespace,
                path: &path_str,
            };
            if let Some(selected) = flowcontrol::resolve::select_for_request(&mut client, &digest).await {
                if let (Ok(fs), Ok(pl)) = (
                    hyper::header::HeaderValue::from_str(&selected.flow_schema_uid),
                    hyper::header::HeaderValue::from_str(&selected.priority_level_uid),
                ) {
                    resp.headers_mut().insert(flowcontrol::resolve::FLOW_SCHEMA_UID_HEADER, fs);
                    resp.headers_mut().insert(flowcontrol::resolve::PRIORITY_LEVEL_UID_HEADER, pl);
                }
            }
        }
    }
    response
}

fn log_audit_event(method: &str, path_str: &str, query: &str, user_agent: Option<&str>, identity: Option<&crate::authn::x509::Identity>, peer: &SocketAddr, status: u16) {
    let event = build_audit_event(method, path_str, query, user_agent, identity, peer, status);
    tracing::info!(target: "nodeapiserver::audit", "{event}");
}

/// The pure half of [`log_audit_event`] — everything up to the built
/// `Value`, factored out so it's unit-testable without capturing
/// `tracing`'s own log output.
fn build_audit_event(method: &str, path_str: &str, query: &str, user_agent: Option<&str>, identity: Option<&crate::authn::x509::Identity>, peer: &SocketAddr, status: u16) -> serde_json::Value {
    let info = path::parse(method, path_str, query);
    let (user_name, user_groups): (&str, Vec<String>) = match identity {
        Some(id) => (id.name.as_str(), id.groups.clone()),
        None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
    };
    let object_ref = info.is_resource_request.then(|| crate::audit::event::ObjectRef { group: &info.api_group, resource: &info.resource, namespace: &info.namespace, name: &info.name, api_version: &info.api_version });
    let request_uri = if query.is_empty() { path_str.to_string() } else { format!("{path_str}?{query}") };
    let audit_id = uuid::Uuid::new_v4().to_string();
    let timestamp = chrono::Utc::now().to_rfc3339();
    let source_ip = peer.ip().to_string();
    crate::audit::event::build_event(&crate::audit::event::EventInput {
        audit_id: &audit_id,
        request_uri: &request_uri,
        verb: &info.verb,
        user_name,
        user_groups: user_groups.as_slice(),
        source_ip: Some(&source_ip),
        user_agent,
        object_ref,
        response_code: status,
        timestamp: &timestamp,
    })
}

fn authenticate_request(
    req: &Request<Incoming>,
    client_cert_identity: Option<crate::authn::x509::Identity>,
    service_account_authenticator: Option<&crate::authn::service_account::Authenticator>,
) -> std::result::Result<Option<crate::authn::x509::Identity>, &'static str> {
    if client_cert_identity.is_some() {
        return Ok(client_cert_identity);
    }
    let Some(header) = req.headers().get("authorization") else {
        return Ok(None);
    };
    let value = header.to_str().map_err(|_| "Authorization header is not valid UTF-8")?;
    let Some(token) = value.strip_prefix("Bearer ").filter(|token| !token.is_empty()) else {
        return Err("Authorization must use the Bearer scheme");
    };
    let Some(authenticator) = service_account_authenticator else {
        return Err("bearer-token authentication is not configured");
    };
    authenticator
        .authenticate(token)
        .map(|authenticated| Some(authenticated.identity))
        .ok_or("bearer token is invalid or expired")
}

async fn handle(
    req: Request<Incoming>,
    storage: Option<StorageClient>,
    cache_registry: crate::cacher::CacheRegistry,
    identity: Option<crate::authn::x509::Identity>,
    service_account_authenticator: Option<Arc<crate::authn::service_account::Authenticator>>,
    enforce_rbac: bool,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Result<Response<BoxedBody>, Infallible> {
    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();

    if let Some(check_name) = path_str.strip_prefix('/').filter(|p| matches!(*p, "healthz" | "readyz" | "livez")) {
        let verbose = path::parse_query(&query).iter().any(|(k, _)| k == "verbose");
        let checks = healthz::run_checks(check_name, storage.is_some());
        let (status, body) = healthz::render(check_name, &checks, verbose);
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return Ok(Response::builder().status(code).header("Content-Type", "text/plain; charset=utf-8").header("X-Content-Type-Options", "nosniff").body(body_from_bytes(body.into_bytes())).unwrap());
    }

    if path_str == "/metrics" {
        return Ok(Response::builder().status(StatusCode::OK).header("Content-Type", "text/plain; version=0.0.4; charset=utf-8").body(body_from_bytes(metrics::render().into_bytes())).unwrap());
    }

    if method == "GET" || method == "HEAD" {
        let parts = path::split_path(&path_str);
        let accept_header = req.headers().get("accept").and_then(|v| v.to_str().ok());
        // Group K: only fetch CRDs for a request that could actually need
        // them — an `/apis`-prefixed path with 3 or fewer segments is
        // exactly `route_discovery`'s own three real `apis`-shaped
        // branches (`/apis`, `/apis/{group}`, `/apis/{group}/{version}`);
        // anything longer is a resource-shaped GET (`/apis/{group}/
        // {version}/namespaces/{ns}/{resource}/...`), which `route_discovery`
        // itself answers `NotApplicable` for and which the generic REST
        // dispatch further down handles instead — that path, by far the
        // hottest one in practice, never pays this extra `LIST`.
        let (crds, aggregated) = if parts.first().map(String::as_str) == Some("apis") && parts.len() <= 3 {
            match storage.clone() {
                Some(mut client) => {
                    let crds = match rest::list_all_crds(&mut client).await {
                        Ok(crds) => crate::apiextensions::registry::discoverable_resources(crds.iter()),
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "discovery: fetching CRDs for the dynamic resource merge failed");
                            Vec::new()
                        }
                    };
                    // Group L Phase 3: the same real gate, the same
                    // bounded cost — only paid for a discovery-shaped
                    // request, never the hot resource-request path.
                    // `discovery::merged_group_version_map`'s own doc
                    // comment covers why this is group-level only.
                    let aggregated = match aggregator::route::discoverable_group_versions(&mut client).await {
                        Ok(pairs) => pairs,
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "discovery: fetching aggregated APIServices for the dynamic group merge failed");
                            Vec::new()
                        }
                    };
                    (crds, aggregated)
                }
                None => (Vec::new(), Vec::new()),
            }
        } else {
            (Vec::new(), Vec::new())
        };
        match route_discovery(&parts, accept_header, &crds, &aggregated) {
            DiscoveryRoute::Found(doc) => return Ok(json_response(StatusCode::OK, &doc)),
            DiscoveryRoute::FoundRaw(bytes) => {
                return Ok(Response::builder().status(StatusCode::OK).header("Content-Type", "application/json").body(body_from_bytes(bytes.to_vec())).unwrap());
            }
            DiscoveryRoute::NotFound => {
                // Group L Phase 3's own last named gap, closed: a real
                // `GET /apis/{group}/{version}` for an aggregated group
                // real upstream itself answers with a *live* fetch to
                // the backend's own discovery endpoint (`checkAPIService`'s
                // own discovery-check dial reused for real traffic too) —
                // this build had no compiled/CRD/discovery-merge answer
                // for that path at all (`discovery::merged_group_version_
                // map`'s own doc comment names this exact gap), so it's
                // the one real case where falling through to `aggregate_
                // proxy` on a `NotFound` (rather than the resource-shaped
                // dispatch's own early check) is correct: `route_discovery`
                // already ruled out every local answer, and `aggregated`
                // (fetched above, same real pre-flight-gated list
                // `aggregate_proxy` itself would recompute) is the one
                // remaining source of truth. Any other `NotFound` (a
                // genuinely unserved group/version) still falls through
                // to the real `404` below unchanged.
                if let Some((group, version)) = aggregated_discovery_group_version(&parts, &aggregated) {
                    if let Some(mut client) = storage.clone() {
                        if let Ok(Some(api_service)) = aggregator::route::resolve(&mut client, group, version).await {
                            return Ok(aggregate_proxy(req, &method, &api_service, client, &path_str, &query).await);
                        }
                    }
                }
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            DiscoveryRoute::NotApplicable => {}
        }
    }

    let info = path::parse(&method, &path_str, &query);

    // Group E's real resource verbs so far: single-object GET (`get`, not
    // `list`/`watch` — `path::parse` already tells those apart by an empty
    // `name`), LIST (`list`, no name), CREATE (`create`, no name — a POST
    // to the collection URL), single-object DELETE (`delete`, name
    // required — no name means `deletecollection`, now real too — see its
    // own dedicated branch below), and UPDATE (`update`, name
    // required — a PUT). No subresource (not
    // handled yet — see `rest`'s own doc comment). Everything else still
    // falls through to the RequestInfo echo below. `storage` is only
    // ever consumed once (moved into `client` here), which is why all
    // five verbs share this one `if let` rather than each checking it
    // separately.
    let is_get = info.is_resource_request && info.verb == "get" && !info.name.is_empty() && info.subresource.is_empty();
    let is_list = info.is_resource_request && info.verb == "list" && info.name.is_empty() && info.subresource.is_empty();
    let is_create = info.is_resource_request && info.verb == "create" && info.name.is_empty() && info.subresource.is_empty();
    let is_delete = info.is_resource_request && info.verb == "delete" && !info.name.is_empty() && info.subresource.is_empty();
    let is_update = info.is_resource_request && info.verb == "update" && !info.name.is_empty() && info.subresource.is_empty();
    // `watch` (no name — `path::parse` already tells a namefull `watch`
    // apart, though real upstream's own single-resource watch form isn't
    // handled specially here either way today) is deliberately handled in
    // its own branch below, not folded into the five-verb block above:
    // unlike those five, it needs no request body, no `storage`/`client`
    // (it's served purely from an already-registered `cacher::CacheRegistry`
    // cache — see that branch's own doc comment), and produces a
    // streaming response rather than one JSON document.
    let is_watch = info.is_resource_request && info.verb == "watch" && info.subresource.is_empty();
    // `PATCH` is handled in its own branch, not folded into the five-verb
    // block below: its request body is a patch document, not a
    // full/partial object, and which of `rest::patch`'s three real patch
    // kinds applies is decided by `Content-Type` rather than the
    // JSON-vs-YAML negotiation `has_body` below uses. Group J admission
    // now runs on it too (`namespace_lifecycle` + `LimitRanger`'s own
    // PVC-update validation — the only two plugins that ever apply to an
    // `Update`-shaped write in this crate; every other Group J plugin is
    // `CREATE`-only, so there's nothing else to run here), via
    // `rest::patch_prepare`/`patch_persist`'s own split, which exists
    // specifically so admission can see the real candidate object in
    // between the two.
    if info.is_resource_request && info.verb == "patch" && !info.name.is_empty() && info.subresource.is_empty() {
        let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);

        // Server-Side Apply — its own branch, not folded into the
        // three-patch-kind block below: `rest::patch_kind_for_content_type`
        // deliberately doesn't recognize this media type (its own doc
        // comment), the body is YAML (or JSON, a valid subset), and the
        // real orchestration (`rest::apply_prepare`/`apply_persist`,
        // Group G's `updater::apply` wired to storage) is a wholly
        // different code path from the three-patch-kind `rest::
        // patch_prepare`/`patch_persist` split above -- but the *same
        // shape* of split, for the same reason: so both
        // `namespace_lifecycle` and `LimitRanger` admission can run
        // against the real candidate object in between, matching the
        // three-patch-kind branch's own coverage exactly. **Named,
        // The runtime-schema CRD path is handled by the same orchestration;
        // schema-less legacy CRD records remain a defensive 501 outcome.
        if content_type.as_deref().map(is_apply_patch_content_type).unwrap_or(false) {
            let Some(mut client) = storage else {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            };
            let Some(manager) = field_manager_query(&query) else {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "the fieldManager query parameter is required for Server-Side Apply")));
            };
            let force = force_query(&query);
            let body_bytes = match read_body_bytes(req).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "reading the request body failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            let config: serde_json::Value = match crate::codec::yaml::decode(&body_bytes) {
                Ok(v) => v,
                Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
            };
            let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };

            // Group J: `namespace_lifecycle`, same `Update`-shaped check
            // every other write-shaped verb gets.
            let admission_attrs = admission::attributes::Attributes { operation: admission::attributes::Operation::Update, group: &info.api_group, resource: &info.resource, namespace: &info.namespace, name: &info.name };
            match admission::namespace_lifecycle::quick_decision(&admission_attrs) {
                admission::namespace_lifecycle::QuickDecision::Allow => {}
                admission::namespace_lifecycle::QuickDecision::Forbidden(msg) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
                }
                admission::namespace_lifecycle::QuickDecision::NeedsNamespaceLookup => {
                    let namespace_phase = match rest::get(&mut client, None, "", "v1", "namespaces", None, &info.namespace).await {
                        Ok(rest::GetOutcome::Found(ns)) => Some(ns.get("status").and_then(|s| s.get("phase")).and_then(|p| p.as_str()).unwrap_or("").to_string()),
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "admission: namespace lookup failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    };
                    match admission::namespace_lifecycle::decide(&admission_attrs, namespace_phase.as_deref()) {
                        admission::namespace_lifecycle::Decision::Allow => {}
                        admission::namespace_lifecycle::Decision::Forbidden(msg) => {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
                        }
                        admission::namespace_lifecycle::Decision::NamespaceNotFound(_) => {
                            return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                        }
                    }
                }
            }

            let (candidate, apply_context) = match rest::apply_prepare(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, &manager, force, &config).await {
                Ok(rest::ApplyPrepareOutcome::Ready(candidate, context)) => (candidate, context),
                Ok(rest::ApplyPrepareOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Ok(rest::ApplyPrepareOutcome::UnsupportedForCrd) => {
                    return Ok(json_response(StatusCode::NOT_IMPLEMENTED, &bad_request_status(&path_str, "Server-Side Apply requires a usable structural schema")));
                }
                Ok(rest::ApplyPrepareOutcome::Conflict(conflicts)) => return Ok(json_response(StatusCode::CONFLICT, &ssa_conflict_status(&path_str, &conflicts))),
                Ok(rest::ApplyPrepareOutcome::Invalid(violations)) => return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                Ok(rest::ApplyPrepareOutcome::NoOp(object)) => return Ok(json_response(StatusCode::OK, &object)),
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "rest::apply_prepare failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };

            // Group J: `LimitRanger`'s own PVC-`Update` validation — the
            // same real candidate object this build's own three-patch-
            // kind `PATCH` branch below already gates the same way (its
            // own comment covers why this is PVC-only).
            if admission::limit_ranger::applies_to(admission::attributes::Operation::Update, &info.api_group, &info.resource, "") {
                match rest::list(&mut client, None, "", "v1", "limitranges", namespace, "", "", 0, "").await {
                    Ok(rest::ListOutcome::Found(list)) => {
                        for limit_range in list["items"].as_array().cloned().unwrap_or_default() {
                            let errs = admission::limit_ranger::validate_pvc(&limit_range, &candidate);
                            if !errs.is_empty() {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &errs.join("; "))));
                            }
                        }
                    }
                    Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "admission: listing limit ranges failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            }

            return match rest::apply_persist(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, apply_context, candidate).await {
                Ok(rest::ApplyOutcome::Applied(object)) => Ok(json_response(StatusCode::OK, &object)),
                Ok(rest::ApplyOutcome::NoOp(object)) => Ok(json_response(StatusCode::OK, &object)),
                Ok(rest::ApplyOutcome::UnknownResource) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Ok(rest::ApplyOutcome::UnsupportedForCrd) => {
                    Ok(json_response(StatusCode::NOT_IMPLEMENTED, &bad_request_status(&path_str, "Server-Side Apply requires a usable structural schema")))
                }
                Ok(rest::ApplyOutcome::Conflict(conflicts)) => Ok(json_response(StatusCode::CONFLICT, &ssa_conflict_status(&path_str, &conflicts))),
                Ok(rest::ApplyOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "rest::apply_persist failed");
                    Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
                }
            };
        }

        let Some(kind_of_patch) = content_type.as_deref().and_then(rest::patch_kind_for_content_type) else {
            return Ok(json_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                &bad_request_status(&path_str, "unsupported or missing Content-Type for PATCH -- use application/json-patch+json, application/merge-patch+json, or application/strategic-merge-patch+json"),
            ));
        };
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let patch_doc: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };

        // Group J: `namespace_lifecycle`, same `Update`-shaped check
        // `CREATE`/`UPDATE` already get (an "operation" of `Update` is
        // exactly right for a `PATCH` too — real upstream's own
        // `admission.Update` covers both).
        let admission_attrs = admission::attributes::Attributes { operation: admission::attributes::Operation::Update, group: &info.api_group, resource: &info.resource, namespace: &info.namespace, name: &info.name };
        match admission::namespace_lifecycle::quick_decision(&admission_attrs) {
            admission::namespace_lifecycle::QuickDecision::Allow => {}
            admission::namespace_lifecycle::QuickDecision::Forbidden(msg) => {
                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
            }
            admission::namespace_lifecycle::QuickDecision::NeedsNamespaceLookup => {
                let namespace_phase = match rest::get(&mut client, None, "", "v1", "namespaces", None, &info.namespace).await {
                    Ok(rest::GetOutcome::Found(ns)) => Some(ns.get("status").and_then(|s| s.get("phase")).and_then(|p| p.as_str()).unwrap_or("").to_string()),
                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "admission: namespace lookup failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                };
                match admission::namespace_lifecycle::decide(&admission_attrs, namespace_phase.as_deref()) {
                    admission::namespace_lifecycle::Decision::Allow => {}
                    admission::namespace_lifecycle::Decision::Forbidden(msg) => {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
                    }
                    admission::namespace_lifecycle::Decision::NamespaceNotFound(_) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                    }
                }
            }
        }

        let (candidate, context) = match rest::patch_prepare(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, kind_of_patch, &patch_doc).await {
            Ok(rest::PatchPrepareOutcome::Ready(candidate, context)) => (candidate, context),
            Ok(rest::PatchPrepareOutcome::UnknownResource) | Ok(rest::PatchPrepareOutcome::ObjectNotFound) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            Ok(rest::PatchPrepareOutcome::Invalid(violations)) => {
                return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations)));
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "rest::patch_prepare failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };

        // Group J: `LimitRanger`'s own PVC-`Update` validation — its only
        // `Update`-shaped check (pods are `CREATE`-only, real upstream's
        // own "containers are immutable after create" posture, see
        // `admission::limit_ranger::applies_to`'s own doc comment).
        if admission::limit_ranger::applies_to(admission::attributes::Operation::Update, &info.api_group, &info.resource, &info.subresource) {
            match rest::list(&mut client, None, "", "v1", "limitranges", namespace, "", "", 0, "").await {
                Ok(rest::ListOutcome::Found(list)) => {
                    for limit_range in list["items"].as_array().cloned().unwrap_or_default() {
                        let errs = admission::limit_ranger::validate_pvc(&limit_range, &candidate);
                        if !errs.is_empty() {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &errs.join("; "))));
                        }
                    }
                }
                Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "admission: listing limit ranges failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }
        }

        return match rest::patch_persist(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, context, candidate).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
            // `rest::patch_persist` never itself returns these two -- a
            // submitted resourceVersion/namespace are `update`-only
            // outcomes, and `UnsupportedPatchType` is pre-checked before
            // `rest::patch_prepare` is ever called. Kept exhaustive rather
            // than `unreachable!()` so a future real use doesn't silently
            // panic in production.
            Ok(rest::UpdateOutcome::MissingResourceVersion) | Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "rest::patch_persist failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }
    // The generic `<resource>/status` subresource — its own branch for
    // the same reason `PATCH` is: the request body here is the caller's
    // view of the *whole* object (typically a GET's own response,
    // status field modified), not a patch document, and only
    // `rest::update_status`'s narrower "replace `.status` only" write
    // applies, not the general five-verb block's `rest::update`. **No
    // Group J admission runs here, named honestly**: every admission
    // plugin that ever applies to an `Update`-shaped write in this crate
    // (`namespace_lifecycle`'s Terminating-namespace check,
    // `LimitRanger`'s PVC-minimum check) is specific to a create/full
    // object write and has nothing meaningful to say about a status-only
    // replace, so there's nothing to wire here yet either — same
    // reasoning `deletecollection`'s own doc comment below already gives
    // for skipping the same two plugins.
    if info.is_resource_request && info.verb == "update" && !info.name.is_empty() && info.subresource == "status" {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };
        return match rest::update_status(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, &body_value).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
            Ok(rest::UpdateOutcome::MissingResourceVersion) => {
                Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.resourceVersion is required for an update")))
            }
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
            // `rest::update_status` never itself returns these three --
            // it runs no structural validation and never checks a body
            // namespace (see its own doc comment), and
            // `UnsupportedPatchType` is `rest::patch`-only. Kept
            // exhaustive rather than `unreachable!()`.
            Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::Invalid(_)) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "rest::update_status failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }
    // `PATCH .../status` — the patch counterpart to the `PUT` branch just
    // above, closing the "PUT-only" gap that branch's own doc comment
    // named. Same no-admission posture as the `PUT` branch (nothing
    // applicable exists for a status-only write); the only new outcome
    // to handle is `Invalid` (a malformed patch document), which
    // `update_status` never itself returns but `rest::patch_status` can.
    if info.is_resource_request && info.verb == "patch" && !info.name.is_empty() && info.subresource == "status" {
        let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        let Some(kind_of_patch) = content_type.as_deref().and_then(rest::patch_kind_for_content_type) else {
            return Ok(json_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                &bad_request_status(&path_str, "unsupported or missing Content-Type for PATCH -- use application/json-patch+json, application/merge-patch+json, or application/strategic-merge-patch+json"),
            ));
        };
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let patch_doc: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };
        return match rest::patch_status(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, kind_of_patch, &patch_doc).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
            // `rest::patch_status` never itself returns these three --
            // no client-submitted `resourceVersion` is required (the
            // object being patched is the one this same call just read,
            // same reasoning `patch_persist` already established), no
            // body namespace is ever checked, and `UnsupportedPatchType`
            // is pre-checked above before `rest::patch_status` is ever
            // called. Kept exhaustive rather than `unreachable!()`.
            Ok(rest::UpdateOutcome::MissingResourceVersion) | Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "rest::patch_status failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }
    // `deletecollection` is handled in its own branch too, for the same
    // reason `patch` is: it needs no request body at all (unlike
    // `create`/`update`), and reuses [`rest::delete_collection`] rather
    // than the single-object shape the five-verb block below assumes.
    // **No Group J admission runs on it yet**, a named gap — but a small
    // one in practice: `namespace_lifecycle`'s own immortal-namespace
    // check needs a `name`, which a collection delete never has, and
    // `LimitRanger`'s only `Update`-shaped check is a PVC *minimum*,
    // which deleting can't violate. See this crate's
    // `rest::delete_collection`'s own doc comment for the rest of its
    // scope.
    if info.is_resource_request && info.verb == "deletecollection" && info.subresource.is_empty() {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };
        return match rest::delete_collection(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.label_selector, &info.field_selector).await {
            Ok(rest::DeleteCollectionOutcome::Deleted(list)) => Ok(json_response(StatusCode::OK, &list)),
            Ok(rest::DeleteCollectionOutcome::UnknownResource) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
            Err(rest::Error::Selector(e)) => Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
            Err(e) => {
                warn!(path = %path_str, error = ?e, "rest::delete_collection failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }
    // Group I: `SubjectAccessReview`/`SelfSubjectAccessReview`/
    // `LocalSubjectAccessReview` — its own branch, checked before the
    // generic `is_create` handling below, because it's a virtual
    // resource: real upstream never persists any of the three kinds to
    // storage (`pkg/registry/authorization/subjectaccessreview`'s own
    // synthetic REST connector), and letting this fall through to the
    // generic `rest::create` path would actually try to write one to
    // nodestore — a real, wrong side effect this early return prevents.
    // Unconditional, not gated by `enforce_rbac`: answering "would RBAC
    // allow this" is a read on the RBAC engine's own state, not itself
    // an enforcement decision.
    if info.is_resource_request
        && info.api_group == "authorization.k8s.io"
        && matches!(info.resource.as_str(), "subjectaccessreviews" | "selfsubjectaccessreviews" | "localsubjectaccessreviews")
        && info.verb == "create"
        && info.subresource.is_empty()
    {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let (fallback_user, fallback_groups): (&str, Vec<String>) = match &identity {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let spec = body_value.get("spec").cloned().unwrap_or_default();
        let mut review = match authz::sar::parse_spec(&spec, fallback_user, &fallback_groups) {
            Ok(r) => r,
            Err(msg) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &msg))),
        };
        // `LocalSubjectAccessReview`'s own real semantics: the namespace
        // is the URL's, not whatever (if anything) the submitted
        // `resourceAttributes.namespace` said — the same "the URL is
        // authoritative over the body" rule `rest::update`'s own
        // namespace-mismatch check already establishes elsewhere in this
        // crate, applied here as an override rather than a rejection
        // since a `LocalSubjectAccessReview` isn't required to name a
        // namespace in its body at all.
        if info.resource == "localsubjectaccessreviews" {
            review.namespace = info.namespace.clone();
        }
        // Non-resource rules are only ever granted via ClusterRoleBindings
        // in real RBAC too (a namespace-scoped RoleBinding can't grant a
        // non-resource-URL permission) -- resolving with an empty
        // namespace naturally restricts to just those, no separate branch
        // needed.
        let resolve_namespace = if review.is_resource { review.namespace.as_str() } else { "" };
        let resolved = authz::resolve::rules_for(&mut client, &review.user_name, &review.user_groups, resolve_namespace).await;
        let attrs = authz::rbac::RequestAttributes {
            is_resource_request: review.is_resource,
            verb: &review.verb,
            api_group: &review.group,
            resource: &review.resource,
            subresource: &review.subresource,
            name: &review.name,
            path: &review.path,
        };
        let allowed = authz::rbac::rules_allow(&attrs, &resolved.rules);
        let mut response_body = body_value;
        response_body["status"] = authz::sar::build_status(allowed);
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // `SelfSubjectRulesReview` — lists the caller's own resolved rules
    // for one namespace, rather than answering a single allow/deny
    // question. Same virtual-resource/no-persistence reasoning as the
    // branch above, its own separate branch only because the response
    // shape (`resourceRules`/`nonResourceRules`, not `allowed`) and
    // input (`spec.namespace`, no attributes to parse) are different
    // enough not to share code cleanly.
    if info.is_resource_request && info.api_group == "authorization.k8s.io" && info.resource == "selfsubjectrulesreviews" && info.verb == "create" && info.subresource.is_empty() {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let (user_name, user_groups): (&str, Vec<String>) = match &identity {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let review_namespace = body_value.pointer("/spec/namespace").and_then(serde_json::Value::as_str).unwrap_or("");
        let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, review_namespace).await;
        let mut response_body = body_value;
        response_body["status"] = authz::sar::build_rules_status(&resolved.rules, &resolved.errors);
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // `SelfSubjectReview` (`kubectl auth whoami`) — the simplest of this
    // crate's virtual resources: no storage, no RBAC, purely reflects
    // whatever identity `authn::x509` (or the real anonymous fallback)
    // already produced. Same "checked before generic `is_create`, never
    // persisted" reasoning as every other review kind above.
    if info.is_resource_request && info.api_group == "authentication.k8s.io" && info.resource == "selfsubjectreviews" && info.verb == "create" && info.subresource.is_empty() {
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let (username, groups): (&str, Vec<String>) = match &identity {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let mut response_body = body_value;
        response_body["status"] = crate::authn::self_review::build_status(username, &groups);
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // Group H: TokenReview is the webhook endpoint nodelet uses when a pod
    // presents its projected ServiceAccount token. It is virtual, just like
    // the authorization review resources above, and must never be written to
    // nodestore.
    if info.is_resource_request
        && info.api_group == "authentication.k8s.io"
        && info.resource == "tokenreviews"
        && info.verb == "create"
        && info.subresource.is_empty()
    {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        if enforce_rbac {
            let (user_name, user_groups): (&str, Vec<String>) = match &identity {
                Some(id) => (id.name.as_str(), id.groups.clone()),
                None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
            };
            let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, "").await;
            let attrs = authz::rbac::RequestAttributes {
                is_resource_request: true,
                verb: "create",
                api_group: &info.api_group,
                resource: &info.resource,
                subresource: &info.subresource,
                name: &info.name,
                path: &path_str,
            };
            if !authz::rbac::rules_allow(&attrs, &resolved.rules) {
                return Ok(json_response(StatusCode::FORBIDDEN, &forbidden_status(&path_str, user_name)));
            }
        }
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the TokenReview body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let mut response_body: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let token = response_body.pointer("/spec/token").and_then(serde_json::Value::as_str).unwrap_or("");
        let authenticated = service_account_authenticator
            .as_deref()
            .and_then(|authenticator| (!token.is_empty()).then(|| authenticator.authenticate(token)).flatten());
        response_body["apiVersion"] = serde_json::json!("authentication.k8s.io/v1");
        response_body["kind"] = serde_json::json!("TokenReview");
        response_body["status"] = match authenticated {
            Some(authenticated) => serde_json::json!({
                "authenticated": true,
                "user": {
                    "username": authenticated.identity.name,
                    "uid": authenticated.service_account_uid,
                    "groups": authenticated.identity.groups,
                }
            }),
            None => serde_json::json!({"authenticated": false}),
        };
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // Group H: ServiceAccount TokenRequest backs projected pod tokens. The
    // caller must be authorized for the serviceaccounts/token subresource;
    // the ServiceAccount and, when supplied, bound Pod are read from storage
    // before the stateless signer is allowed to mint a token.
    if info.is_resource_request
        && info.api_group.is_empty()
        && info.resource == "serviceaccounts"
        && info.subresource == "token"
        && info.verb == "create"
        && !info.namespace.is_empty()
        && !info.name.is_empty()
    {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        if enforce_rbac {
            let (user_name, user_groups): (&str, Vec<String>) = match &identity {
                Some(id) => (id.name.as_str(), id.groups.clone()),
                None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
            };
            let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, &info.namespace).await;
            let attrs = authz::rbac::RequestAttributes {
                is_resource_request: true,
                verb: "create",
                api_group: "",
                resource: "serviceaccounts",
                subresource: "token",
                name: &info.name,
                path: &path_str,
            };
            if !authz::rbac::rules_allow(&attrs, &resolved.rules) {
                return Ok(json_response(StatusCode::FORBIDDEN, &forbidden_status(&path_str, user_name)));
            }
        }
        let Some(authenticator) = service_account_authenticator.as_deref() else {
            return Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(&path_str, "ServiceAccount token signing is not configured")));
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the TokenRequest body failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let request = match crate::authn::service_account::parse_token_request(&body_value) {
            Ok(request) => request,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e))),
        };
        let service_account = match rest::get(&mut client, None, "", "v1", "serviceaccounts", Some(&info.namespace), &info.name).await {
            Ok(rest::GetOutcome::Found(service_account)) => service_account,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "TokenRequest ServiceAccount lookup failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let service_account_uid = service_account
            .pointer("/metadata/uid")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if let Some((pod_name, pod_uid)) = &request.bound_pod {
            match rest::get(&mut client, None, "", "v1", "pods", Some(&info.namespace), pod_name).await {
                Ok(rest::GetOutcome::Found(pod)) if pod.pointer("/metadata/uid").and_then(serde_json::Value::as_str) == Some(pod_uid) => {}
                Ok(rest::GetOutcome::Found(_)) => {
                    return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "bound Pod UID does not match the current Pod")));
                }
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                    return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                }
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "TokenRequest bound Pod lookup failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }
        }
        let issued = match authenticator.issue_token(&info.namespace, &info.name, service_account_uid, &request) {
            Ok(issued) => issued,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let mut response_body = body_value;
        response_body["apiVersion"] = serde_json::json!("authentication.k8s.io/v1");
        response_body["kind"] = serde_json::json!("TokenRequest");
        response_body["status"] = serde_json::json!({
            "token": issued.token,
            "expirationTimestamp": issued.expiration_timestamp,
        });
        return Ok(json_response(StatusCode::CREATED, &response_body));
    }
    // Group L: aggregated APIs (`APIService`) — a genuine live reverse
    // proxy to a real aggregated backend now, Phase 4's remaining wiring.
    // Checked before the generic verb dispatch (every other special-cased
    // route in this function is too), and before `pods/log` right below
    // since an aggregated group could in principle define its own `pods`
    // resource — the check itself costs nothing extra for the vastly more
    // common non-aggregated request (`aggregator::route::resolve` is a
    // bounded `LIST` of `APIService`s only, not per-item I/O, and a
    // request with an empty `api_group` — the core group — short-circuits
    // inside it immediately). See `aggregator::route`/`::client_tls`/
    // `::availability`/`::proxy_target`'s own doc comments for the full
    // design; `aggregate_proxy` below is the dispatch glue, same split
    // `pods/log`'s own branch already established. **Discovery merge
    // (Phase 3) is still not done** — an aggregated group's own
    // `/apis/{group}/{version}` discovery document isn't proxied yet, a
    // real, separate, named gap (`aggregator::mod`'s own doc comment);
    // only resource-shaped requests under an already-known `(group,
    // version)` reach this branch at all, matching real upstream's own
    // "resource requests only" scope for its aggregation proxy handler.
    if info.is_resource_request && !info.api_group.is_empty() {
        if let Some(mut client) = storage.clone() {
            match aggregator::route::resolve(&mut client, &info.api_group, &info.api_version).await {
                Ok(Some(api_service)) => return Ok(aggregate_proxy(req, &method, &api_service, client, &path_str, &query).await),
                Ok(None) => {}
                Err(e) => warn!(path = %path_str, error = ?e, "aggregation: looking up a matching APIService failed"),
            }
        }
    }
    // Group N: pod connection subresources are HTTP upgrades. Resolve the
    // pod and its node here, then let the streaming proxy carry the upgrade
    // through to nodelet. This must run before the generic REST branches:
    // `POST .../pods/{name}/exec` is otherwise indistinguishable from an
    // ordinary create-shaped request to the path parser.
    if info.is_resource_request
        && info.api_group.is_empty()
        && info.resource == "pods"
        && !info.name.is_empty()
        && matches!(info.subresource.as_str(), "exec" | "attach" | "portforward")
        && matches!(method.as_str(), "GET" | "POST")
    {
        let Some(mut client) = storage.clone() else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };

        if enforce_rbac {
            let (user_name, user_groups): (&str, Vec<String>) = match &identity {
                Some(id) => (id.name.as_str(), id.groups.clone()),
                None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
            };
            let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, &info.namespace).await;
            let attrs = authz::rbac::RequestAttributes {
                is_resource_request: true,
                verb: &info.verb,
                api_group: &info.api_group,
                resource: &info.resource,
                subresource: &info.subresource,
                name: &info.name,
                path: &path_str,
            };
            if !authz::rbac::rules_allow(&attrs, &resolved.rules) {
                return Ok(json_response(StatusCode::FORBIDDEN, &forbidden_status(&path_str, user_name)));
            }
        }

        let pod = match rest::get(&mut client, None, "", "v1", "pods", namespace, &info.name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "proxy: fetching the pod for a streaming subresource failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let node_name = pod.pointer("/spec/nodeName").and_then(serde_json::Value::as_str).unwrap_or("");
        if node_name.is_empty() {
            return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "pod has not yet been scheduled to a node")));
        }
        let node = match rest::get(&mut client, None, "", "v1", "nodes", None, node_name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
            Err(error) => {
                warn!(path = %path_str, node = %node_name, error = ?error, "proxy: fetching the pod node for a streaming subresource failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };

        let pairs = path::parse_query(&query);
        let target = match proxy::pod_stream::target(&pod, &node, &info.subresource, &pairs) {
            Ok(target) => target,
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::NoDefaultContainer { pod_name, candidates })) => {
                let detail = if candidates.is_empty() {
                    format!("a container name must be specified for pod {pod_name}")
                } else {
                    format!("a container name must be specified for pod {pod_name}, choose one of: {candidates:?}")
                };
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &detail)));
            }
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::UnknownContainer { pod_name, container })) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &format!("container {container} is not valid for pod {pod_name}"))));
            }
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::PodNotScheduled)) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "pod has not yet been scheduled to a node")));
            }
            Err(proxy::pod_stream::Error::Pod(proxy::pod_log::Error::NoNodeAddress)) => {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
            Err(proxy::pod_stream::Error::MissingPort) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "at least one port is required for port-forward")));
            }
            Err(proxy::pod_stream::Error::InvalidPort(port)) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &format!("invalid port {port}"))));
            }
        };

        return match proxy::http_client::upgrade(req, &target, kubelet_tls).await {
            Ok(response) => Ok(response),
            Err(error) => {
                warn!(path = %path_str, node = %node_name, error = ?error, "proxy: streaming upgrade to nodelet failed");
                Ok(json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(&path_str, &error.to_string())))
            }
        };
    }
    // Group N: `pods/log` — a genuine live proxy to nodelet's own
    // `/containerLogs` endpoint (`crates/nodelet/src/server/logs.rs`),
    // not a stub. See `proxy::pod_log`/`proxy::client_tls`/
    // `proxy::http_client`'s own doc comments for the full design; this
    // branch is just the dispatch glue: fetch the pod, fetch its node,
    // resolve the target (`proxy::pod_log::log_location`), dial nodelet
    // for real, relay its response — status, headers, streaming body —
    // back unmodified. Checked before the generic `is_get` handling below
    // (which requires an empty `subresource`), same "specific virtual/
    // special-cased routes before the generic verb block" ordering every
    // other early-return branch above already uses.
    if info.is_resource_request && info.api_group.is_empty() && info.resource == "pods" && info.subresource == "log" && !info.name.is_empty() && (method == "GET" || method == "HEAD") {
        let Some(mut client) = storage.clone() else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };

        // Group I: `pods/log` is its own distinct resource for RBAC
        // purposes -- a role granting `get` on `pods` does NOT imply
        // `pods/log`, real upstream's own subresource-is-a-separate-
        // resource rule -- so this checks the subresource explicitly
        // rather than reusing whatever the plain `pods` `get` branch
        // below would decide.
        if enforce_rbac {
            let (user_name, user_groups): (&str, Vec<String>) = match &identity {
                Some(id) => (id.name.as_str(), id.groups.clone()),
                None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
            };
            let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, &info.namespace).await;
            let attrs = authz::rbac::RequestAttributes {
                is_resource_request: true,
                verb: "get",
                api_group: &info.api_group,
                resource: &info.resource,
                subresource: &info.subresource,
                name: &info.name,
                path: &path_str,
            };
            if !authz::rbac::rules_allow(&attrs, &resolved.rules) {
                return Ok(json_response(StatusCode::FORBIDDEN, &forbidden_status(&path_str, user_name)));
            }
        }

        let pod = match rest::get(&mut client, None, "", "v1", "pods", namespace, &info.name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "proxy: fetching the pod for pods/log failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let node_name = pod.get("spec").and_then(|s| s.get("nodeName")).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
        if node_name.is_empty() {
            return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "pod has not yet been scheduled to a node")));
        }
        // Nodes are cluster-scoped -- `namespace: None`, matching every
        // other cluster-scoped `rest::get` call in this module.
        let node = match rest::get(&mut client, None, "", "v1", "nodes", None, &node_name).await {
            Ok(rest::GetOutcome::Found(object)) => object,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                warn!(path = %path_str, node = %node_name, "proxy: pod's own node not found for pods/log");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "proxy: fetching the pod's node for pods/log failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };

        let query_pairs = path::parse_query(&query);
        let container = query_pairs.iter().find(|(k, _)| k == "container").map(|(_, v)| v.clone()).unwrap_or_default();
        let target = match proxy::pod_log::log_location(&pod, &node, &container, &query_pairs) {
            Ok(t) => t,
            Err(proxy::pod_log::Error::NoDefaultContainer { pod_name, candidates }) => {
                let detail = if candidates.is_empty() {
                    format!("a container name must be specified for pod {pod_name}")
                } else {
                    format!("a container name must be specified for pod {pod_name}, choose one of: {candidates:?}")
                };
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &detail)));
            }
            Err(proxy::pod_log::Error::UnknownContainer { pod_name, container }) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &format!("container {container} is not valid for pod {pod_name}"))));
            }
            Err(proxy::pod_log::Error::PodNotScheduled) => {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "pod has not yet been scheduled to a node")));
            }
            Err(proxy::pod_log::Error::NoNodeAddress) => {
                warn!(path = %path_str, node = %node_name, "proxy: node has no address of any preferred type for pods/log");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };

        return match proxy::http_client::fetch(&target, kubelet_tls).await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                warn!(path = %path_str, node = %node_name, error = ?e, "proxy: dialing nodelet for pods/log failed");
                Ok(json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(&path_str, &e.to_string())))
            }
        };
    }

    let has_body = is_create || is_update;
    if is_get || is_list || is_create || is_delete || is_update {
        // Captured before `req` is potentially consumed below (`has_body`
        // moves it into `read_body_bytes`) — a borrow of `req.headers()`
        // can't outlive that move.
        let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        // Same reasoning — `GET`/`LIST`'s own `Table` negotiation
        // (`kubectl get`'s real default `Accept` header) needs this
        // after `req` may already be gone.
        let wants_table = req.headers().get("accept").and_then(|v| v.to_str().ok()).and_then(negotiation::negotiate).map(|a| a.wants_table()).unwrap_or(false);

        if let Some(mut client) = storage {
            let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };

            // Decode the body once, shared by CREATE and UPDATE (both
            // take a full submitted object), per its real negotiated
            // Content-Type. JSON/YAML only for now — a protobuf request
            // body would need the target schema to decode, which needs
            // the resource resolved first; named honestly as a real,
            // separate gap rather than guessed at (see `rest`'s own
            // module doc comment).
            let mut body_value = if has_body {
                let body_bytes = match read_body_bytes(req).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "reading the request body failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                };
                let format = content_type.as_deref().and_then(negotiation::content_type).unwrap_or(negotiation::Format::Json);
                let decoded: Result<serde_json::Value, String> = match format {
                    negotiation::Format::Json => crate::codec::json::decode(&body_bytes).map_err(|e| e.to_string()),
                    negotiation::Format::Yaml => crate::codec::yaml::decode(&body_bytes).map_err(|e| e.to_string()),
                    negotiation::Format::Protobuf => {
                        return Ok(json_response(
                            StatusCode::UNSUPPORTED_MEDIA_TYPE,
                            &bad_request_status(&path_str, "protobuf request bodies are not decoded yet for CREATE/UPDATE — use application/json or application/yaml"),
                        ));
                    }
                };
                match decoded {
                    Ok(v) => Some(v),
                    Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e))),
                }
            } else {
                None
            };

            // Group J: mutating admission — `DefaultTolerationSeconds`,
            // real upstream's own plugin, ported (see
            // `admission::default_toleration_seconds`'s own doc comment).
            // Unconditional, same posture as `namespace_lifecycle`: no
            // bootstrap data needed, so no lockout risk to gate behind a
            // config flag. Runs on the decoded body before it reaches
            // `rest::create`/`update`, so the appended tolerations are
            // part of what actually gets validated and persisted.
            if let Some(body) = body_value.as_mut() {
                if admission::default_toleration_seconds::applies_to(&info.api_group, &info.resource, &info.subresource) {
                    admission::default_toleration_seconds::mutate(body);
                }
            }

            // Group J: `ServiceAccount` — mutating + validating, `CREATE`
            // only (see `admission::service_account`'s own doc comment for
            // exactly what's ported and what's named-honestly skipped).
            // Defaulting is pure and always runs on a `pods` CREATE;
            // `quick_decision` then says whether a real `ServiceAccount`
            // lookup is needed before this plugin can finish.
            if is_create {
                if let Some(pod) = body_value.as_mut() {
                    if admission::service_account::applies_to(&info.api_group, &info.resource, &info.subresource) {
                        admission::service_account::default_service_account_name(pod);
                        match admission::service_account::quick_decision(pod, admission::attributes::Operation::Create) {
                            admission::service_account::Decision::Allow => {}
                            admission::service_account::Decision::Forbidden(msg) => {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
                            }
                            admission::service_account::Decision::NeedsServiceAccountLookup => {
                                let sa_name = pod.get("spec").and_then(|s| s.get("serviceAccountName")).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                                match rest::get(&mut client, None, "", "v1", "serviceaccounts", namespace, &sa_name).await {
                                    Ok(rest::GetOutcome::Found(sa)) => {
                                        admission::service_account::mutate_with_service_account(pod, &sa, || {
                                            let suffix: String = uuid::Uuid::new_v4().to_string().chars().take(5).collect();
                                            format!("{}{suffix}", admission::service_account::SERVICE_ACCOUNT_VOLUME_PREFIX)
                                        });
                                    }
                                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                                        return Ok(json_response(
                                            StatusCode::FORBIDDEN,
                                            &admission_forbidden_status(&path_str, &format!("error looking up service account {:?}/{sa_name:?}: not found", info.namespace)),
                                        ));
                                    }
                                    Err(e) => {
                                        warn!(path = %path_str, error = ?e, "admission: service account lookup failed");
                                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Group J: `DefaultStorageClass` — mutating, `CREATE` only
            // (see `admission::default_storage_class`'s own doc comment).
            // Unlike `namespace_lifecycle`/`service_account`, this one has
            // no cheap `QuickDecision`-style early-out before the one real
            // I/O step: `mutate` itself checks whether the PVC already has
            // a class and no-ops, but only after the `StorageClass` list
            // has already been fetched — a real (small) inefficiency for
            // the common already-classed case, named honestly rather than
            // silently optimized around with a duplicated has-class check.
            if is_create {
                if let Some(pvc) = body_value.as_mut() {
                    if admission::default_storage_class::applies_to(&info.api_group, &info.resource, &info.subresource) {
                        match rest::list(&mut client, None, "storage.k8s.io", "v1", "storageclasses", None, "", "", 0, "").await {
                            Ok(rest::ListOutcome::Found(list)) => {
                                let classes = list["items"].as_array().cloned().unwrap_or_default();
                                admission::default_storage_class::mutate(pvc, &classes);
                            }
                            Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {
                                // This build's own discovery table doesn't
                                // know `storageclasses` at all — treat the
                                // same as "no default class exists" rather
                                // than failing the PVC create, matching
                                // upstream's own "no default class
                                // selected, do nothing" no-op path.
                            }
                            Err(e) => {
                                warn!(path = %path_str, error = ?e, "admission: listing storage classes failed");
                                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                            }
                        }
                    }
                }
            }

            // Group J: `LimitRanger` — mutating (pods only, `CREATE` only)
            // + validating (pods and PVCs; see
            // `admission::limit_ranger`'s own doc comment for exact scope
            // and what's not yet ported). `operation` mirrors the same
            // three-way mapping the other Group J blocks each compute
            // locally.
            {
                let operation = if is_create {
                    Some(admission::attributes::Operation::Create)
                } else if is_update {
                    Some(admission::attributes::Operation::Update)
                } else if is_delete {
                    Some(admission::attributes::Operation::Delete)
                } else {
                    None
                };
                if let Some(operation) = operation {
                    if admission::limit_ranger::applies_to(operation, &info.api_group, &info.resource, &info.subresource) {
                        match rest::list(&mut client, None, "", "v1", "limitranges", namespace, "", "", 0, "").await {
                            Ok(rest::ListOutcome::Found(list)) => {
                                let limit_ranges = list["items"].as_array().cloned().unwrap_or_default();
                                if let Some(body) = body_value.as_mut() {
                                    if is_create && info.resource == "pods" {
                                        admission::limit_ranger::mutate_pod(body, &limit_ranges);
                                    }
                                    for limit_range in &limit_ranges {
                                        let errs = if info.resource == "pods" {
                                            admission::limit_ranger::validate_pod(limit_range, body)
                                        } else {
                                            admission::limit_ranger::validate_pvc(limit_range, body)
                                        };
                                        if !errs.is_empty() {
                                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &errs.join("; "))));
                                        }
                                    }
                                }
                            }
                            Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {
                                // No `limitranges` known to this build at
                                // all — same "nothing to enforce" no-op as
                                // an empty list.
                            }
                            Err(e) => {
                                warn!(path = %path_str, error = ?e, "admission: listing limit ranges failed");
                                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                            }
                        }
                    }
                }
            }

            // Group J: `PodSecurity` — validating, `CREATE` only (see
            // `admission::pod_security`'s own doc comment for exactly
            // which checks are ported and which are named, honest gaps).
            // The one real I/O step: fetch the target namespace to read
            // its own `pod-security.kubernetes.io/enforce` label.
            if is_create && admission::pod_security::applies_to(&info.api_group, &info.resource, &info.subresource, admission::attributes::Operation::Create) {
                if let Some(pod) = body_value.as_ref() {
                    match rest::get(&mut client, None, "", "v1", "namespaces", None, &info.namespace).await {
                        Ok(rest::GetOutcome::Found(ns)) => {
                            let level = admission::pod_security::enforcement_level(&ns);
                            let violations = admission::pod_security::validate(pod, level);
                            if !violations.is_empty() {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &violations.join("; "))));
                            }
                        }
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                            // No real namespace to read a label off —
                            // `namespace_lifecycle` is what's responsible
                            // for rejecting a create into a namespace that
                            // doesn't exist at all; this check just has
                            // nothing to enforce in that case.
                        }
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "admission: namespace lookup for PodSecurity failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                }
            }

            // Group J: `ResourceQuota` — validating, `CREATE` only, pods/
            // PVCs/services only (see `admission::resource_quota`'s own
            // doc comment for the full, honestly-named scope). Runs last
            // among the mutating-then-validating admission blocks above,
            // same relative position real upstream's own default plugin
            // order uses (quota checks the final, fully-defaulted/mutated
            // object) — placed after `LimitRanger`'s own defaulting, so a
            // container that only got its requests/limits from a
            // `LimitRange` default is still counted correctly here. Two
            // real I/O steps: list every existing object of the same kind
            // already in the namespace (to sum existing usage) and every
            // `ResourceQuota` in it.
            let quota_kind = if !is_create {
                None
            } else if admission::resource_quota::applies_to(admission::attributes::Operation::Create, &info.api_group, &info.resource, &info.subresource) {
                Some("pods")
            } else if admission::resource_quota::applies_to_pvc(admission::attributes::Operation::Create, &info.api_group, &info.resource, &info.subresource) {
                Some("persistentvolumeclaims")
            } else if admission::resource_quota::applies_to_service(admission::attributes::Operation::Create, &info.api_group, &info.resource, &info.subresource) {
                Some("services")
            } else {
                None
            };
            // Populated for the pod/PVC/service evaluators (the generic
            // object-count evaluator doesn't persist its own status.used
            // yet — a named follow-up), consumed after `rest::create`
            // actually succeeds below. Computing this here (before
            // creation) rather than re-listing after
            // is deliberate: it's the exact same existing-usage snapshot
            // `check_pod_create` just used to allow the request, so the
            // two stay consistent with each other.
            let mut quota_usage_updates: Vec<(String, std::collections::BTreeMap<String, crate::scheme::quantity::Quantity>)> = Vec::new();
            if let Some(list_resource) = quota_kind {
                if let Some(new_object) = body_value.as_ref() {
                    let existing = match rest::list(&mut client, None, "", "v1", list_resource, namespace, "", "", 0, "").await {
                        Ok(rest::ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
                        Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => Vec::new(),
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, resource = list_resource, "admission: listing existing objects for ResourceQuota failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    };
                    match rest::list(&mut client, None, "", "v1", "resourcequotas", namespace, "", "", 0, "").await {
                        Ok(rest::ListOutcome::Found(list)) => {
                            let quotas = list["items"].as_array().cloned().unwrap_or_default();
                            let denial = match list_resource {
                                "pods" => admission::resource_quota::check_pod_create(new_object, &existing, &quotas),
                                "persistentvolumeclaims" => admission::resource_quota::check_pvc_create(new_object, &existing, &quotas),
                                _ => admission::resource_quota::check_service_create(new_object, &existing, &quotas),
                            };
                            if let Some(denial) = denial {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &denial)));
                            }
                            quota_usage_updates = match list_resource {
                                "pods" => admission::resource_quota::usage_after_pod_create(new_object, &existing, &quotas),
                                "persistentvolumeclaims" => admission::resource_quota::usage_after_pvc_create(new_object, &existing, &quotas),
                                _ => admission::resource_quota::usage_after_service_create(new_object, &existing, &quotas),
                            };
                        }
                        Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {
                            // No `resourcequotas` known to this build —
                            // same "nothing to enforce" no-op as an empty
                            // list.
                        }
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "admission: listing resource quotas failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                }
            } else if is_create && !info.namespace.is_empty() {
                // Group J: `ResourceQuota`'s generic object-count
                // evaluator (`admission::resource_quota::check_object_count_create`'s
                // own doc comment) — runs for any namespaced resource
                // `CREATE` that isn't already covered by the pod/PVC/
                // service evaluators above (a real, deliberate skip, not
                // an oversight: those three already track their own
                // legacy bare-name object count). Safe to run
                // unconditionally: a namespace with no `ResourceQuota`
                // referencing this resource's `count/...` key has
                // nothing to enforce.
                let existing = match rest::list(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, "", "", 0, "").await {
                    Ok(rest::ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
                    Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => Vec::new(),
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "admission: listing existing objects for ResourceQuota's object-count check failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                };
                match rest::list(&mut client, None, "", "v1", "resourcequotas", namespace, "", "", 0, "").await {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let quotas = list["items"].as_array().cloned().unwrap_or_default();
                        if let Some(denial) = admission::resource_quota::check_object_count_create(&info.api_group, &info.resource, &existing, &quotas) {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &denial)));
                        }
                        quota_usage_updates = admission::resource_quota::usage_after_object_count_create(&info.api_group, &info.resource, &existing, &quotas);
                    }
                    Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "admission: listing resource quotas failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            }

            // Group I: authorization, opt-in (see config::Config::enforce_rbac's
            // own doc comment for why this defaults to off rather than
            // being unconditional the moment identity extraction and RBAC
            // resolution both exist). A request with no established x509
            // identity is evaluated as the real anonymous user/group
            // upstream itself uses, not silently skipped.
            if enforce_rbac {
                let (user_name, user_groups): (&str, Vec<String>) = match &identity {
                    Some(id) => (id.name.as_str(), id.groups.clone()),
                    None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
                };
                let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, &info.namespace).await;
                let attrs = authz::rbac::RequestAttributes {
                    is_resource_request: true,
                    verb: &info.verb,
                    api_group: &info.api_group,
                    resource: &info.resource,
                    subresource: &info.subresource,
                    name: &info.name,
                    path: "",
                };
                if !authz::rbac::rules_allow(&attrs, &resolved.rules) {
                    return Ok(json_response(StatusCode::FORBIDDEN, &forbidden_status(&path_str, user_name)));
                }
            }

            // Group J: admission control, unconditional — see
            // `admission`'s own doc comment for why this plugin, unlike
            // Group I's RBAC, needs no config gate (it needs no
            // operator-provisioned bootstrap data, so there's no
            // "could lock every request out" risk). Only the three
            // mutating verbs pass through a real admission plugin at all;
            // GET/LIST are unaffected, matching real upstream (admission
            // only ever runs on write operations).
            if is_create || is_update || is_delete {
                let operation = if is_create {
                    admission::attributes::Operation::Create
                } else if is_update {
                    admission::attributes::Operation::Update
                } else {
                    admission::attributes::Operation::Delete
                };
                let admission_attrs = admission::attributes::Attributes { operation, group: &info.api_group, resource: &info.resource, namespace: &info.namespace, name: &info.name };

                match admission::namespace_lifecycle::quick_decision(&admission_attrs) {
                    admission::namespace_lifecycle::QuickDecision::Allow => {}
                    admission::namespace_lifecycle::QuickDecision::Forbidden(msg) => {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
                    }
                    admission::namespace_lifecycle::QuickDecision::NeedsNamespaceLookup => {
                        // `namespaces` is cluster-scoped — looked up by
                        // name with no parent namespace, same convention
                        // every other cluster-scoped `get` in this crate
                        // uses.
                        let namespace_phase = match rest::get(&mut client, None, "", "v1", "namespaces", None, &info.namespace).await {
                            Ok(rest::GetOutcome::Found(ns)) => Some(ns.get("status").and_then(|s| s.get("phase")).and_then(|p| p.as_str()).unwrap_or("").to_string()),
                            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                            Err(e) => {
                                warn!(path = %path_str, error = ?e, "admission: namespace lookup failed");
                                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                            }
                        };
                        match admission::namespace_lifecycle::decide(&admission_attrs, namespace_phase.as_deref()) {
                            admission::namespace_lifecycle::Decision::Allow => {}
                            admission::namespace_lifecycle::Decision::Forbidden(msg) => {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &msg)));
                            }
                            admission::namespace_lifecycle::Decision::NamespaceNotFound(_) => {
                                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                            }
                        }
                    }
                }
            }

            // `BOOT_CACHED_RESOURCES` (`run()`'s own doc comment) has a
            // real cache registered; every other resource still gets
            // `None` from `cache_registry.get`, same as before any cache
            // existed. Shared by both verbs below; `rest::list`'s own doc
            // comment covers why an unsynced cache is safe to pass here
            // too (it just falls through, same as `None`).
            let resource_cache = cache_registry.get(&info.api_group, &info.api_version, &info.resource);
            let resource_cache = resource_cache.as_ref();

            if is_get {
                match rest::get(&mut client, resource_cache, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                    Ok(rest::GetOutcome::Found(object)) => {
                        let body = if wants_table { crate::codec::table::convert_to_table(&object) } else { object };
                        return Ok(json_response(StatusCode::OK, &body));
                    }
                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                    }
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::get failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            } else if is_list {
                match rest::list(&mut client, resource_cache, &info.api_group, &info.api_version, &info.resource, namespace, &info.label_selector, &info.field_selector, info.limit, &info.continue_token).await {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let body = if wants_table { crate::codec::table::convert_to_table(&list) } else { list };
                        return Ok(json_response(StatusCode::OK, &body));
                    }
                    Ok(rest::ListOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                    Ok(rest::ListOutcome::InvalidContinueToken) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "continue token is not valid")));
                    }
                    // A malformed selector is the client's fault, not a
                    // server failure — real upstream answers this with a
                    // 400, not a 500.
                    Err(rest::Error::Selector(e)) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::list failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            } else if is_create {
                // `has_body` guarantees this is `Some` — the decode
                // happened above, before this branch was even chosen.
                let body_value = body_value.expect("body_value is Some whenever is_create is true (has_body covers it)");
                match rest::create(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &body_value).await {
                    Ok(rest::CreateOutcome::Created(object)) => {
                        // Group J: persist `ResourceQuota.status.used` now
                        // that the object this usage total was computed
                        // for is genuinely real. Best-effort — a status
                        // write failing here must never turn an already-
                        // succeeded create into an error response; the
                        // request was correctly admitted regardless of
                        // whether its bookkeeping write lands.
                        if let Some(ns) = namespace {
                            persist_quota_usage_updates(&mut client, ns, quota_usage_updates, &path_str).await;
                        }
                        return Ok(json_response(StatusCode::CREATED, &object));
                    }
                    Ok(rest::CreateOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                    Ok(rest::CreateOutcome::MissingName) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.name is required (generateName is not supported)")));
                    }
                    Ok(rest::CreateOutcome::NamespaceMismatch) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.namespace does not match the request URL")));
                    }
                    Ok(rest::CreateOutcome::AlreadyExists) => return Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
                    Ok(rest::CreateOutcome::Invalid(violations)) => return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::create failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            } else if is_update {
                let body_value = body_value.expect("body_value is Some whenever is_update is true (has_body covers it)");
                match rest::update(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, &body_value).await {
                    Ok(rest::UpdateOutcome::Updated(object)) => return Ok(json_response(StatusCode::OK, &object)),
                    Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                    }
                    Ok(rest::UpdateOutcome::MissingResourceVersion) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.resourceVersion is required for an update")));
                    }
                    Ok(rest::UpdateOutcome::NamespaceMismatch) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.namespace does not match the request URL")));
                    }
                    Ok(rest::UpdateOutcome::Conflict) => return Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
                    Ok(rest::UpdateOutcome::Invalid(violations)) => return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                    // `rest::update` never itself returns this -- it's
                    // `rest::patch`-only, checked before `rest::patch` is
                    // even called (see the `PATCH` branch above). Kept
                    // exhaustive rather than `unreachable!()`.
                    Ok(rest::UpdateOutcome::UnsupportedPatchType) => return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str))),
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::update failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            } else {
                // is_delete.
                match rest::delete(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                    Ok(rest::DeleteOutcome::Deleted(object)) => return Ok(json_response(StatusCode::OK, &object)),
                    Ok(rest::DeleteOutcome::ObjectNotFound) | Ok(rest::DeleteOutcome::UnknownResource) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                    }
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::delete failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            }
        }
        // No nodestore connection at all (failed at startup, or not yet
        // reconnected) — falls through to the echo stub below rather than
        // claiming a 503 for a request this build genuinely can't judge
        // "not found" vs. "unreachable" for yet.
    }

    // Group D/E: real `WATCH`, served purely from an already-registered
    // `cacher::CacheRegistry` cache — see `BOOT_CACHED_RESOURCES` for
    // which resources that is today. A live cache already holds
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
        if enforce_rbac {
            let (user_name, user_groups): (&str, Vec<String>) = match &identity {
                Some(id) => (id.name.as_str(), id.groups.clone()),
                None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
            };
            match storage.clone() {
                Some(mut client) => {
                    let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, &info.namespace).await;
                    let attrs = authz::rbac::RequestAttributes {
                        is_resource_request: true,
                        verb: &info.verb,
                        api_group: &info.api_group,
                        resource: &info.resource,
                        subresource: &info.subresource,
                        name: &info.name,
                        path: "",
                    };
                    if !authz::rbac::rules_allow(&attrs, &resolved.rules) {
                        return Ok(json_response(StatusCode::FORBIDDEN, &forbidden_status(&path_str, user_name)));
                    }
                }
                None => {
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }
        }
        // Group K: an already-registered cache first (unchanged), else —
        // only when the static table doesn't know this resource at all —
        // a live check against the dynamic CRD registry, lazily spawning
        // a cache for it right now on this, its first-ever watch request
        // (`cacher::registry::CacheRegistry::spawn` is callable at any
        // time, not just at boot — see its own doc comment). A real
        // built-in resource simply outside `BOOT_CACHED_RESOURCES` still
        // gets no watch support, exactly as before Group K existed — only
        // a resource the static table has never heard of falls through to
        // the dynamic check, so this never masks a genuine 404 as "maybe
        // a CRD." **Named, honest scope**: nothing proactively reacts to
        // a CRD's own lifecycle (becoming `Established`, or being
        // deleted) — a CRD deleted after its resource was ever watched
        // once leaves an idle reflector running for the rest of this
        // process's life, real upstream's own per-CRD informer teardown
        // isn't modeled yet.
        let cache_and_kind: Option<(crate::cacher::store::SharedCache, String)> = if let Some(cache) = cache_registry.get(&info.api_group, &info.api_version, &info.resource) {
            rest::resolve_kind(&info.api_group, &info.api_version, &info.resource).map(|kind| (cache, kind.to_string()))
        } else if rest::resolve_kind(&info.api_group, &info.api_version, &info.resource).is_some() {
            None
        } else if let Some(mut client) = storage.clone() {
            match rest::resolve_dynamic_kind(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                Ok(Some(kind)) => Some((cache_registry.spawn(client, &info.api_group, &info.api_version, &info.resource), kind)),
                Ok(None) => None,
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "watch: resolving a possible CRD-defined resource failed");
                    None
                }
            }
        } else {
            None
        };

        if let Some((cache, kind)) = cache_and_kind {
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
            let start_revision = resource_version_query(&query);
            match cache.watch_from(start_revision) {
                Ok((replay, rx)) => {
                    let group_version = if info.api_group.is_empty() { info.api_version.clone() } else { format!("{}/{}", info.api_group, info.api_version) };
                    let body = watch_response_body(replay, rx, kind, group_version, label_reqs, field_reqs, storage.clone(), info.api_group.clone(), info.resource.clone(), info.api_version.clone());
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

    // Surfaced for real observability (this is the only response shape
    // that ever includes it today), not consulted for any access-control
    // decision anywhere yet — there is no authorization (Group I) to
    // enforce it against. `rest::get`/`list` above don't take it either,
    // for the same reason: nothing yet checks a caller's identity before
    // serving a read.
    let user = identity.as_ref().map(|i| serde_json::json!({"username": i.name, "groups": i.groups}));
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
    Ok(json_response(StatusCode::OK, &value))
}

/// Request headers this build never forwards to an aggregated backend —
/// hop-by-hop headers (`Connection`'s own listed value plus the fixed
/// standard set, RFC 7230 §6.1) and `Host` (rebuilt from the resolved
/// target instead, same as `proxy::http_client::fetch`'s own posture for
/// nodelet).
const HOP_BY_HOP_HEADERS: &[&str] = &["host", "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailers", "transfer-encoding", "upgrade"];

/// Group L Phase 4's dispatch glue for one already-matched, non-local
/// `APIService`: fetch its backing Service and `EndpointSlice`s, run the
/// same real pre-flight chain `aggregator::availability::preflight_check`
/// would run before a live discovery-endpoint dial, resolve the actual
/// dial target (`aggregator::proxy_target`), build this backend's own
/// TLS trust (`aggregator::client_tls`), and relay the whole request —
/// method, headers minus [`HOP_BY_HOP_HEADERS`], body — unmodified
/// (`proxy::http_client::relay`). A real transparent proxy, matching
/// real upstream's own aggregation posture exactly: nothing about the
/// request or response is inspected or altered beyond what dialing
/// itself requires.
///
/// A cached `Available: False` condition (`aggregator::reconcile`'s own
/// periodic write, `availability::cached_available`) short-circuits
/// straight to `503` before any of the Service/`EndpointSlice` I/O below
/// — a known-broken backend fails fast without paying for a fetch this
/// build already knows the answer to. `Available: True` or no cached
/// condition yet both fall through to the full check unchanged (the
/// backing Service still has to be fetched either way, to resolve the
/// actual dial target — this only ever saves the *negative* path).
async fn aggregate_proxy(req: Request<Incoming>, method: &str, api_service: &serde_json::Value, mut client: StorageClient, path_str: &str, query: &str) -> Response<BoxedBody> {
    if aggregator::availability::cached_available(api_service) == Some(false) {
        return json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(path_str, "backing service is not currently available (cached)"));
    }
    let Some(service_ref) = api_service.pointer("/spec/service") else {
        // `aggregator::route::resolve` already filters this out -- reached
        // only if the stored object changed between that check and here.
        return json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, "APIService has no backing service"));
    };
    let namespace = service_ref.get("namespace").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    let name = service_ref.get("name").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    let port = service_ref.get("port").and_then(serde_json::Value::as_i64).unwrap_or(443);

    let service = match rest::get(&mut client, None, "", "v1", "services", Some(&namespace), &name).await {
        Ok(rest::GetOutcome::Found(object)) => Some(object),
        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
        Err(e) => {
            warn!(path = %path_str, error = ?e, "aggregation: fetching the backing service failed");
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
        }
    };
    let endpoint_slices = match rest::list(&mut client, None, "discovery.k8s.io", "v1", "endpointslices", Some(&namespace), &format!("kubernetes.io/service-name={name}"), "", 0, "").await {
        Ok(rest::ListOutcome::Found(list)) => list.get("items").and_then(serde_json::Value::as_array).cloned().unwrap_or_default(),
        Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => Vec::new(),
        Err(e) => {
            warn!(path = %path_str, error = ?e, "aggregation: listing endpointslices for the backing service failed");
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
        }
    };
    if let Err(condition) = aggregator::availability::preflight_check(&namespace, &name, port, service.as_ref(), &endpoint_slices) {
        warn!(path = %path_str, reason = condition.reason, message = %condition.message, "aggregation: pre-flight check failed, not attempting the backend dial");
        return json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(path_str, &condition.message));
    }
    let Some(service) = service else {
        return json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(path_str, "backing service not found"));
    };

    let target = match aggregator::proxy_target::resolve(api_service, &service, path_str, query) {
        Ok(t) => t,
        Err(aggregator::proxy_target::Error::Local) => return json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, "APIService has no backing service")),
        Err(aggregator::proxy_target::Error::NoClusterIp) => return json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, "backing service has no clusterIP to dial")),
    };

    let insecure_skip_tls_verify = api_service.pointer("/spec/insecureSkipTLSVerify").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let ca_bundle_pem = match api_service.pointer("/spec/caBundle").and_then(serde_json::Value::as_str) {
        Some(b64) if !b64.is_empty() => {
            use base64::Engine;
            match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "aggregation: spec.caBundle is not valid base64");
                    return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
                }
            }
        }
        _ => None,
    };
    let client_config = match aggregator::client_tls::build_client_config(ca_bundle_pem.as_deref(), insecure_skip_tls_verify) {
        Ok(cfg) => std::sync::Arc::new(cfg),
        Err(e) => {
            warn!(path = %path_str, error = ?e, "aggregation: building the backend TLS client config failed");
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
        }
    };

    let headers = req
        .headers()
        .iter()
        .filter(|(name, _)| !HOP_BY_HOP_HEADERS.contains(&name.as_str().to_ascii_lowercase().as_str()))
        .filter_map(|(name, value)| value.to_str().ok().map(|v| (name.as_str().to_string(), v.to_string())))
        .collect::<Vec<_>>();
    let body = match read_body_bytes(req).await {
        Ok(b) => b,
        Err(e) => {
            warn!(path = %path_str, error = ?e, "aggregation: reading the request body failed");
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
        }
    };

    match proxy::http_client::relay(&target, client_config, method, &headers, body).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(path = %path_str, host = %target.host, error = ?e, "aggregation: dialing the backend failed");
            json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, &e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(path: &str) -> Vec<String> {
        path::split_path(path)
    }

    #[test]
    fn api_root_serves_api_versions() {
        let route = route_discovery(&parts("/api"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIVersions");
    }

    #[test]
    fn api_v1_serves_the_core_group_resource_list() {
        let route = route_discovery(&parts("/api/v1"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIResourceList");
        assert_eq!(doc["groupVersion"], "v1");
    }

    #[test]
    fn apis_root_serves_the_group_list() {
        let route = route_discovery(&parts("/apis"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIGroupList");
    }

    #[test]
    fn apis_root_serves_aggregated_discovery_when_the_client_asks_for_it() {
        let accept = "application/json;as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io";
        let route = route_discovery(&parts("/apis"), Some(accept), &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIGroupDiscoveryList");
    }

    #[test]
    fn api_root_serves_aggregated_discovery_when_the_client_asks_for_it() {
        let accept = "application/json;as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io";
        let route = route_discovery(&parts("/api"), Some(accept), &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIGroupDiscoveryList");
        assert_eq!(doc["items"][0]["metadata"]["name"], "");
    }

    #[test]
    fn a_mismatched_as_version_falls_back_to_the_legacy_shape() {
        // v2beta1 is real upstream's pre-GA aggregated-discovery shape,
        // which this build doesn't separately model — must not be served
        // the v2 shape as if it matched.
        let accept = "application/json;as=APIGroupDiscoveryList;v=v2beta1;g=apidiscovery.k8s.io";
        let route = route_discovery(&parts("/apis"), Some(accept), &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIGroupList", "an unmatched as= version must fall back to the legacy shape, not silently serve v2 anyway");
    }

    #[test]
    fn apis_group_serves_the_group_document() {
        let route = route_discovery(&parts("/apis/apps"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIGroup");
        assert_eq!(doc["name"], "apps");
    }

    #[test]
    fn apis_group_version_serves_the_resource_list() {
        let route = route_discovery(&parts("/apis/apps/v1"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIResourceList");
        assert_eq!(doc["groupVersion"], "apps/v1");
    }

    #[test]
    fn aggregated_discovery_group_version_matches_a_real_apis_group_version_path() {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        assert_eq!(aggregated_discovery_group_version(&parts("/apis/metrics.k8s.io/v1beta1"), &aggregated), Some(("metrics.k8s.io", "v1beta1")));
    }

    #[test]
    fn aggregated_discovery_group_version_is_none_for_a_group_not_in_the_aggregated_list() {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        assert_eq!(aggregated_discovery_group_version(&parts("/apis/apps/v1"), &aggregated), None);
    }

    #[test]
    fn aggregated_discovery_group_version_requires_exactly_three_apis_segments() {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        assert_eq!(aggregated_discovery_group_version(&parts("/apis/metrics.k8s.io"), &aggregated), None, "a group-only path must not match");
        assert_eq!(
            aggregated_discovery_group_version(&parts("/apis/metrics.k8s.io/v1beta1/nodes"), &aggregated),
            None,
            "a resource-shaped path is handled by the resource-request aggregation branch, not this one"
        );
    }

    #[test]
    fn aggregated_discovery_group_version_ignores_a_matching_version_under_a_different_top_level_prefix() {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        assert_eq!(aggregated_discovery_group_version(&parts("/api/metrics.k8s.io/v1beta1"), &aggregated), None);
    }

    #[test]
    fn an_unknown_group_is_a_real_not_found_not_a_fallthrough() {
        assert!(matches!(route_discovery(&parts("/apis/totally.made.up"), None, &[], &[]), DiscoveryRoute::NotFound));
        assert!(matches!(route_discovery(&parts("/apis/apps/v999"), None, &[], &[]), DiscoveryRoute::NotFound));
        assert!(matches!(route_discovery(&parts("/api/v999"), None, &[], &[]), DiscoveryRoute::NotFound));
    }

    #[test]
    fn a_resource_shaped_path_is_not_applicable_to_discovery_routing() {
        assert!(matches!(route_discovery(&parts("/api/v1/namespaces/default/pods"), None, &[], &[]), DiscoveryRoute::NotApplicable));
        assert!(matches!(route_discovery(&parts("/apis/apps/v1/namespaces/default/deployments"), None, &[], &[]), DiscoveryRoute::NotApplicable));
        assert!(matches!(route_discovery(&parts("/"), None, &[], &[]), DiscoveryRoute::NotApplicable));
    }

    #[test]
    fn openapi_v3_root_serves_the_root_index() {
        let route = route_discovery(&parts("/openapi/v3"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert!(doc["paths"].as_object().unwrap().contains_key("apis/apps/v1"));
    }

    #[test]
    fn openapi_v3_a_multi_segment_path_serves_the_raw_vendored_document() {
        let route = route_discovery(&parts("/openapi/v3/apis/apps/v1"), None, &[], &[]);
        let DiscoveryRoute::FoundRaw(bytes) = route else { panic!("expected FoundRaw") };
        let parsed: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert!(parsed.get("openapi").is_some());
    }

    #[test]
    fn openapi_v3_an_unvendored_path_is_a_real_not_found() {
        assert!(matches!(route_discovery(&parts("/openapi/v3/apis/totally.made.up/v1"), None, &[], &[]), DiscoveryRoute::NotFound));
    }

    #[test]
    fn version_serves_the_real_version_info_document() {
        let route = route_discovery(&parts("/version"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert!(doc.get("gitVersion").is_some());
    }

    #[test]
    fn not_found_status_has_the_real_client_go_status_shape() {
        let status = not_found_status("/apis/totally.made.up");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["apiVersion"], "v1");
        assert_eq!(status["status"], "Failure");
        assert_eq!(status["reason"], "NotFound");
        assert_eq!(status["code"], 404);
    }

    #[test]
    fn bad_request_status_carries_the_selector_parse_detail() {
        let status = bad_request_status("/api/v1/pods", "malformed selector");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "BadRequest");
        assert_eq!(status["code"], 400);
        assert!(status["message"].as_str().unwrap().contains("malformed selector"));
    }

    #[test]
    fn forbidden_status_names_the_user_and_uses_the_real_rbac_denial_shape() {
        let status = forbidden_status("/api/v1/pods", "system:anonymous");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "Forbidden");
        assert_eq!(status["code"], 403);
        assert!(status["message"].as_str().unwrap().contains("system:anonymous"));
    }

    #[test]
    fn conflict_status_uses_the_real_already_exists_shape() {
        let status = conflict_status("/api/v1/namespaces/default/pods");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "AlreadyExists");
        assert_eq!(status["code"], 409);
    }

    #[test]
    fn invalid_status_joins_every_violation_into_the_message() {
        let status = invalid_status("/api/v1/pods", &["spec.containers: Required value".to_string(), "spec.foo: expected type string, got number".to_string()]);
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "Invalid");
        assert_eq!(status["code"], 422);
        let message = status["message"].as_str().unwrap();
        assert!(message.contains("spec.containers: Required value"));
        assert!(message.contains("spec.foo: expected type string, got number"));
    }

    #[test]
    fn resource_expired_status_uses_the_real_gone_shape() {
        let status = resource_expired_status("/api/v1/watch/pods");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "Gone");
        assert_eq!(status["code"], 410);
    }

    #[test]
    fn resource_version_query_reads_the_real_param() {
        assert_eq!(resource_version_query("resourceVersion=42"), 42);
        assert_eq!(resource_version_query("watch=true&resourceVersion=7&timeoutSeconds=30"), 7);
    }

    #[test]
    fn resource_version_query_defaults_to_zero_when_absent_or_unparsable() {
        assert_eq!(resource_version_query(""), 0);
        assert_eq!(resource_version_query("watch=true"), 0);
        assert_eq!(resource_version_query("resourceVersion=not-a-number"), 0);
    }

    #[test]
    fn resource_version_query_handles_a_percent_encoded_value() {
        // Real clients never percent-encode a bare integer, but some
        // generic HTTP tooling does anyway — defensive, not a case real
        // kubectl/client-go traffic would ever hit.
        assert_eq!(resource_version_query("resourceVersion=%34%32"), 42);
    }

    #[test]
    fn is_apply_patch_content_type_recognizes_the_real_media_type_and_ignores_charset() {
        assert!(is_apply_patch_content_type("application/apply-patch+yaml"));
        assert!(is_apply_patch_content_type("application/apply-patch+yaml; charset=utf-8"));
        assert!(!is_apply_patch_content_type("application/strategic-merge-patch+json"));
        assert!(!is_apply_patch_content_type(""));
    }

    #[test]
    fn field_manager_query_reads_the_real_param() {
        assert_eq!(field_manager_query("fieldManager=kubectl-apply"), Some("kubectl-apply".to_string()));
        assert_eq!(field_manager_query("force=true&fieldManager=kubectl-apply"), Some("kubectl-apply".to_string()));
        assert_eq!(field_manager_query(""), None);
        assert_eq!(field_manager_query("force=true"), None);
    }

    #[test]
    fn force_query_reads_the_real_param() {
        assert!(force_query("force=true"));
        assert!(force_query("fieldManager=x&force=true"));
        assert!(!force_query(""));
        assert!(!force_query("force=false"));
        assert!(!force_query("force=1"));
    }

    #[test]
    fn ssa_conflict_status_names_every_conflicting_manager() {
        let mut fields = crate::patch::fieldset::Set::new();
        fields.insert(&[crate::patch::fieldset::PathElement::Field("replicas".to_string())]);
        let conflicts = vec![crate::patch::updater::Conflict { manager: "hpa-controller".to_string(), fields }];
        let status = ssa_conflict_status("/apis/apps/v1/namespaces/default/deployments/my-app", &conflicts);
        assert_eq!(status["code"], 409);
        assert_eq!(status["reason"], "Conflict");
        assert!(status["message"].as_str().unwrap().contains("hpa-controller"));
    }

    #[test]
    fn encode_watch_event_produces_a_newline_terminated_json_line() {
        let event = crate::cacher::store::WatchEvent { kind: crate::cacher::store::EventKind::Bookmark, key: Vec::new(), value: Vec::new(), revision: 9 };
        let frame = encode_watch_event(&event, "Pod", "v1", None, "", "pods", "v1").expect("Bookmark always converts").expect("Bookmark conversion never fails");
        let bytes = frame.into_data().unwrap();
        assert!(bytes.ends_with(b"\n"));
        let parsed: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(parsed["type"], "BOOKMARK");
        assert_eq!(parsed["object"]["kind"], "Pod");
    }

    #[test]
    fn encode_watch_event_skips_a_deleted_event_with_no_retained_value() {
        let event = crate::cacher::store::WatchEvent { kind: crate::cacher::store::EventKind::Deleted, key: b"k".to_vec(), value: Vec::new(), revision: 9 };
        assert!(encode_watch_event(&event, "Pod", "v1", None, "", "pods", "v1").is_none());
    }

    fn envelope_for(name: &str, labels: serde_json::Value) -> Vec<u8> {
        let schema = crate::codec::protobuf::schema_for_gvk("", "v1", "Namespace").unwrap();
        let object_bytes = crate::codec::protobuf::encode_message(schema, &serde_json::json!({"metadata": {"name": name, "labels": labels}})).unwrap();
        crate::codec::protobuf::wrap_unknown("v1", "Namespace", &object_bytes)
    }

    #[test]
    fn watch_event_matches_selector_passes_bookmarks_and_valueless_events_through() {
        let reqs = crate::cacher::selector::parse_label_selector("env=prod").unwrap();
        let bookmark = crate::cacher::store::WatchEvent { kind: crate::cacher::store::EventKind::Bookmark, key: Vec::new(), value: Vec::new(), revision: 1 };
        assert!(watch_event_matches_selector(&bookmark, &reqs, &[], None, "", ""));
    }

    #[test]
    fn watch_event_matches_selector_filters_on_labels() {
        let reqs = crate::cacher::selector::parse_label_selector("env=prod").unwrap();
        let matching = crate::cacher::store::WatchEvent { kind: crate::cacher::store::EventKind::Added, key: b"a".to_vec(), value: envelope_for("a", serde_json::json!({"env": "prod"})), revision: 1 };
        let non_matching = crate::cacher::store::WatchEvent { kind: crate::cacher::store::EventKind::Added, key: b"b".to_vec(), value: envelope_for("b", serde_json::json!({"env": "dev"})), revision: 2 };
        assert!(watch_event_matches_selector(&matching, &reqs, &[], None, "", ""));
        assert!(!watch_event_matches_selector(&non_matching, &reqs, &[], None, "", ""));
    }

    #[test]
    fn watch_event_matches_selector_is_a_no_op_with_no_selector() {
        let event = crate::cacher::store::WatchEvent { kind: crate::cacher::store::EventKind::Added, key: b"a".to_vec(), value: envelope_for("a", serde_json::json!({})), revision: 1 };
        assert!(watch_event_matches_selector(&event, &[], &[], None, "", ""));
    }

    #[tokio::test]
    async fn watch_response_body_streams_the_replay_then_live_events() {
        use http_body_util::BodyExt;

        // An unrelated event at revision 2 first, purely so `watch_from`'s
        // own "not older than the oldest retained history entry" check
        // has something at or before the requested start_revision (same
        // pre-existing `watch_from` quirk `cacher::store`'s own tests hit
        // — untouched by, and unrelated to, what this test is proving).
        // The event actually under test needs a real encoded envelope —
        // `to_watch_event_json` decodes it for real, same as
        // `server::watch_event`'s own tests do.
        let schema = crate::codec::protobuf::schema_for_gvk("", "v1", "Namespace").unwrap();
        let object_bytes = crate::codec::protobuf::encode_message(schema, &serde_json::json!({"metadata": {"name": "default"}})).unwrap();
        let envelope = crate::codec::protobuf::wrap_unknown("v1", "Namespace", &object_bytes);

        let cache = crate::cacher::store::WatchCache::new(vec![], 1, 16, 16);
        let shared = crate::cacher::store::SharedCache::new(cache);
        shared.apply(crate::cacher::store::EventKind::Added, b"seed".to_vec(), b"unrelated".to_vec(), 2);
        shared.apply(crate::cacher::store::EventKind::Added, b"a".to_vec(), envelope, 3);
        let (replay, rx) = shared.watch_from(2).unwrap();
        assert_eq!(replay.len(), 1, "only the revision-3 event should be in the replay");
        // Drop the cache (and its own broadcast::Sender) before consuming
        // the stream to completion below — otherwise the live half of
        // `watch_response_body` never ends (a real watch stream is
        // meant to run forever; only exercised for the replay half here,
        // the live half is real end-to-end behavior, not something a
        // `.collect()`-to-completion unit test can observe without
        // artificially closing the channel first).
        drop(shared);

        let body = watch_response_body(replay, rx, "Namespace".to_string(), "v1".to_string(), Vec::new(), Vec::new(), None, String::new(), "namespaces".to_string(), "v1".to_string());
        let collected = body.collect().await.unwrap().to_bytes();
        let text = String::from_utf8(collected.to_vec()).unwrap();
        assert_eq!(text.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["type"], "ADDED");
    }

    fn test_peer() -> SocketAddr {
        "10.0.0.7:54321".parse().unwrap()
    }

    #[test]
    fn build_audit_event_carries_the_real_request_shape_for_an_anonymous_user() {
        let event = build_audit_event("GET", "/api/v1/namespaces/default/pods/web-1", "", None, None, &test_peer(), 200);
        assert_eq!(event["verb"], "get");
        assert_eq!(event["user"]["username"], "system:anonymous");
        assert_eq!(event["responseStatus"]["code"], 200);
        assert_eq!(event["sourceIPs"], serde_json::json!(["10.0.0.7"]));
        assert_eq!(event["objectRef"]["resource"], "pods");
        assert_eq!(event["objectRef"]["namespace"], "default");
        assert_eq!(event["objectRef"]["name"], "web-1");
    }

    #[test]
    fn build_audit_event_carries_the_real_identity_when_present() {
        let identity = crate::authn::x509::Identity { name: "alice".to_string(), groups: vec!["developers".to_string()], credential_id: (String::new(), Vec::new()) };
        let event = build_audit_event("GET", "/api/v1/pods", "watch=true", None, Some(&identity), &test_peer(), 200);
        assert_eq!(event["user"]["username"], "alice");
        assert_eq!(event["user"]["groups"], serde_json::json!(["developers"]));
        assert_eq!(event["verb"], "watch");
        assert_eq!(event["requestURI"], "/api/v1/pods?watch=true");
    }

    #[test]
    fn build_audit_event_has_no_object_ref_for_a_non_resource_request() {
        let event = build_audit_event("GET", "/version", "", None, None, &test_peer(), 200);
        assert!(event.get("objectRef").is_none());
    }

    #[test]
    fn build_audit_event_carries_a_denied_response_code() {
        let event = build_audit_event("DELETE", "/api/v1/namespaces/default/pods/web-1", "", None, None, &test_peer(), 403);
        assert_eq!(event["responseStatus"]["code"], 403);
    }
}
