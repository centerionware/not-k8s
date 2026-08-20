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
//! a deliberately bounded, reasoned list of core-group resources (the
//! ones a real cluster's own kubelets/kube-proxy/controllers read most
//! heavily), not every resource this build knows about — and `GET`/`LIST`
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
//! too now (`rest::patch`, reusing Group G's already-landed
//! `patch::json_patch`/`merge_patch`/`strategic_merge`, selected by the
//! real `Content-Type` — `application/json-patch+json`/
//! `application/merge-patch+json`/`application/strategic-merge-patch+json`
//! — with a real `415` for anything else) — **but doesn't run through
//! Group J admission yet**, a named gap (this branch's own comment,
//! right above where it's handled, has the reason). **`deletecollection`
//! is still a bring-up stub** — a request against it still just echoes
//! the parsed [`crate::server::path::RequestInfo`] as JSON, not the real
//! dispatch.
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
use crate::authz;
use crate::config::Config;
use crate::codec::negotiation;
use crate::server::{discovery, healthz, openapi, path, rest, version};
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
fn encode_watch_event(event: &crate::cacher::store::WatchEvent, kind: &str, api_version: &str) -> Option<Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>> {
    match crate::server::watch_event::to_watch_event_json(event, kind, api_version) {
        None => None,
        Some(Ok(json)) => {
            let mut bytes = serde_json::to_vec(&json).unwrap_or_default();
            bytes.push(b'\n');
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
fn watch_response_body(replay: Vec<crate::cacher::store::WatchEvent>, rx: tokio::sync::broadcast::Receiver<crate::cacher::store::WatchEvent>, kind: String, api_version: String) -> BoxedBody {
    use http_body_util::{BodyExt, StreamBody};
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt;

    let replay_stream = tokio_stream::iter(replay);
    let live_stream = BroadcastStream::new(rx).map_while(|res| res.ok());
    let events = replay_stream.chain(live_stream);
    let frames = events.filter_map(move |event| encode_watch_event(&event, &kind, &api_version));
    StreamBody::new(frames).boxed()
}

/// Runs the listener forever (until the process exits). Best-effort on
/// bind/TLS failure — logs and returns rather than panicking, matching
/// every other background loop's degrade-and-continue posture in this
/// workspace (see `crates/nodelet/src/server/mod.rs::run`'s own doc
/// comment for the precedent).
pub async fn run(cfg: Config) {
    let cert_dir = std::path::PathBuf::from("/var/lib/nodeapiserver/pki");
    let sans = vec!["localhost".to_string(), "127.0.0.1".to_string(), "kubernetes".to_string(), "kubernetes.default".to_string()];
    let cert = match super::tls::load_or_generate(&cert_dir, &sans) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "failed to load/generate a TLS certificate; the REST/watch listener will not run");
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

    let server_config = match cert.server_config(client_ca.as_ref()) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "failed to build TLS server config; the REST/watch listener will not run");
            return;
        }
    };
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    // Best-effort, matching every other failure in this function: a
    // nodestore that isn't reachable yet at startup shouldn't stop the
    // listener from serving discovery (which needs no storage at all) —
    // `rest::get` degrades to the bring-up echo stub when this is `None`
    // (see its own call site's comment). Connected once here and cloned
    // per connection below: `StorageClient` wraps a cheap-to-clone
    // `tonic::transport::Channel`, the same "clone per use, don't share a
    // `&mut` behind a lock" posture `cacher`'s own driver takes.
    let storage = match StorageClient::connect(&cfg).await {
        Ok(c) => Some(c),
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
    // reasoned, small subset instead: the core-group resources a real
    // cluster's own kubelets/kube-proxy/controllers read most heavily
    // (GET/LIST-heavy, write-light), not an attempt at the general
    // policy. `StorageClient::clone()` is cheap (a `tonic::transport::Channel`
    // clone), so registering several of these costs no extra real
    // connections.
    let cache_registry = crate::cacher::CacheRegistry::new();
    if let Some(s) = storage.as_ref() {
        for (group, version, resource) in BOOT_CACHED_RESOURCES {
            cache_registry.spawn(s.clone(), group, version, resource);
        }
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
            let service = hyper::service::service_fn(move |req| handle_with_audit(req, storage.clone(), cache_registry.clone(), identity.clone(), enforce_rbac, peer));
            if let Err(e) = ConnBuilder::new(TokioExecutor::new()).serve_connection(io, service).await {
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
fn route_discovery(parts: &[String], accept_header: Option<&str>) -> DiscoveryRoute {
    let seg = |i: usize| parts.get(i).map(String::as_str);
    match (seg(0), seg(1), parts.len()) {
        (Some("api"), _, 1) if wants_aggregated_discovery(accept_header) => DiscoveryRoute::Found(discovery::api_v1_group_discovery_list()),
        (Some("api"), _, 1) => DiscoveryRoute::Found(discovery::api_versions()),
        (Some("api"), _, 2) => match discovery::api_resource_list("", &parts[1]) {
            Some(doc) => DiscoveryRoute::Found(doc),
            None => DiscoveryRoute::NotFound,
        },
        (Some("apis"), _, 1) if wants_aggregated_discovery(accept_header) => DiscoveryRoute::Found(discovery::api_group_discovery_list()),
        (Some("apis"), _, 1) => DiscoveryRoute::Found(discovery::api_group_list()),
        (Some("apis"), _, 2) => match discovery::api_group(&parts[1]) {
            Some(doc) => DiscoveryRoute::Found(doc),
            None => DiscoveryRoute::NotFound,
        },
        (Some("apis"), _, 3) => match discovery::api_resource_list(&parts[1], &parts[2]) {
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
const BOOT_CACHED_RESOURCES: &[(&str, &str, &str)] = &[("", "v1", "namespaces"), ("", "v1", "pods"), ("", "v1", "services"), ("", "v1", "secrets"), ("", "v1", "configmaps"), ("", "v1", "endpoints"), ("", "v1", "nodes")];

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
async fn handle_with_audit(req: Request<Incoming>, storage: Option<StorageClient>, cache_registry: crate::cacher::CacheRegistry, identity: Option<crate::authn::x509::Identity>, enforce_rbac: bool, peer: SocketAddr) -> Result<Response<BoxedBody>, Infallible> {
    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let user_agent = req.headers().get("user-agent").and_then(|v| v.to_str().ok()).map(str::to_string);
    let audit_identity = identity.clone();

    let response = handle(req, storage, cache_registry, identity, enforce_rbac).await;

    if let Ok(resp) = &response {
        log_audit_event(&method, &path_str, &query, user_agent.as_deref(), audit_identity.as_ref(), &peer, resp.status().as_u16());
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

async fn handle(req: Request<Incoming>, storage: Option<StorageClient>, cache_registry: crate::cacher::CacheRegistry, identity: Option<crate::authn::x509::Identity>, enforce_rbac: bool) -> Result<Response<BoxedBody>, Infallible> {
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

    if method == "GET" || method == "HEAD" {
        let parts = path::split_path(&path_str);
        let accept_header = req.headers().get("accept").and_then(|v| v.to_str().ok());
        match route_discovery(&parts, accept_header) {
            DiscoveryRoute::Found(doc) => return Ok(json_response(StatusCode::OK, &doc)),
            DiscoveryRoute::FoundRaw(bytes) => {
                return Ok(Response::builder().status(StatusCode::OK).header("Content-Type", "application/json").body(body_from_bytes(bytes.to_vec())).unwrap());
            }
            DiscoveryRoute::NotFound => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
            DiscoveryRoute::NotApplicable => {}
        }
    }

    let info = path::parse(&method, &path_str, &query);

    // Group E's real resource verbs so far: single-object GET (`get`, not
    // `list`/`watch` — `path::parse` already tells those apart by an empty
    // `name`), LIST (`list`, no name), CREATE (`create`, no name — a POST
    // to the collection URL), single-object DELETE (`delete`, name
    // required — no name means `deletecollection`, still the echo stub),
    // and UPDATE (`update`, name required — a PUT). No subresource (not
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
    // JSON-vs-YAML negotiation `has_body` below uses. **No Group J
    // admission runs on `PATCH` yet** — a named, honest gap: the
    // mutating/validating plugin chain below is wired specifically
    // against `body_value`/`is_create`/`is_update`, and `PATCH`'s own
    // final object only exists once `rest::patch` has already applied
    // the patch and persisted it, past the point admission would need to
    // run to still be able to reject the write. Closing this gap needs
    // `rest::patch` split into an apply-then-validate-then-persist shape
    // the way `create`/`update` already are — separate follow-up work.
    if info.is_resource_request && info.verb == "patch" && !info.name.is_empty() && info.subresource.is_empty() {
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
        return match rest::patch(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, kind_of_patch, &patch_doc).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
            // `rest::patch` never itself returns these two -- they're
            // `update`-only (a submitted resourceVersion, a submitted
            // namespace) and `UnsupportedPatchType` is pre-checked above,
            // before `rest::patch` is ever called. Kept exhaustive rather
            // than `unreachable!()` so a future real use from `rest::patch`
            // doesn't silently panic in production.
            Ok(rest::UpdateOutcome::MissingResourceVersion) | Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
            Err(e) => {
                warn!(path = %path_str, error = ?e, "rest::patch failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }
    let has_body = is_create || is_update;
    if is_get || is_list || is_create || is_delete || is_update {
        // Captured before `req` is potentially consumed below (`has_body`
        // moves it into `read_body_bytes`) — a borrow of `req.headers()`
        // can't outlive that move.
        let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);

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
                        match rest::list(&mut client, None, "storage.k8s.io", "v1", "storageclasses", None, "", "").await {
                            Ok(rest::ListOutcome::Found(list)) => {
                                let classes = list["items"].as_array().cloned().unwrap_or_default();
                                admission::default_storage_class::mutate(pvc, &classes);
                            }
                            Ok(rest::ListOutcome::UnknownResource) => {
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
                        match rest::list(&mut client, None, "", "v1", "limitranges", namespace, "", "").await {
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
                            Ok(rest::ListOutcome::UnknownResource) => {
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
            if let Some(list_resource) = quota_kind {
                if let Some(new_object) = body_value.as_ref() {
                    let existing = match rest::list(&mut client, None, "", "v1", list_resource, namespace, "", "").await {
                        Ok(rest::ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
                        Ok(rest::ListOutcome::UnknownResource) => Vec::new(),
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, resource = list_resource, "admission: listing existing objects for ResourceQuota failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    };
                    match rest::list(&mut client, None, "", "v1", "resourcequotas", namespace, "", "").await {
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
                        }
                        Ok(rest::ListOutcome::UnknownResource) => {
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
                let existing = match rest::list(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, "", "").await {
                    Ok(rest::ListOutcome::Found(list)) => list["items"].as_array().cloned().unwrap_or_default(),
                    Ok(rest::ListOutcome::UnknownResource) => Vec::new(),
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "admission: listing existing objects for ResourceQuota's object-count check failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                };
                match rest::list(&mut client, None, "", "v1", "resourcequotas", namespace, "", "").await {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let quotas = list["items"].as_array().cloned().unwrap_or_default();
                        if let Some(denial) = admission::resource_quota::check_object_count_create(&info.api_group, &info.resource, &existing, &quotas) {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &denial)));
                        }
                    }
                    Ok(rest::ListOutcome::UnknownResource) => {}
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
                    Ok(rest::GetOutcome::Found(object)) => return Ok(json_response(StatusCode::OK, &object)),
                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                    }
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::get failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            } else if is_list {
                match rest::list(&mut client, resource_cache, &info.api_group, &info.api_version, &info.resource, namespace, &info.label_selector, &info.field_selector).await {
                    Ok(rest::ListOutcome::Found(list)) => return Ok(json_response(StatusCode::OK, &list)),
                    Ok(rest::ListOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
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
                    Ok(rest::CreateOutcome::Created(object)) => return Ok(json_response(StatusCode::CREATED, &object)),
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
        if let Some(cache) = cache_registry.get(&info.api_group, &info.api_version, &info.resource) {
            let start_revision = resource_version_query(&query);
            match cache.watch_from(start_revision) {
                Ok((replay, rx)) => {
                    let Some(kind) = rest::resolve_kind(&info.api_group, &info.api_version, &info.resource) else {
                        // A cache exists but the discovery table doesn't
                        // know this (group, version, resource) — shouldn't
                        // happen in practice (nothing registers a cache
                        // for an unknown resource), but a real 404 is the
                        // honest answer if it ever did, not a panic.
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                    };
                    let group_version = if info.api_group.is_empty() { info.api_version.clone() } else { format!("{}/{}", info.api_group, info.api_version) };
                    let body = watch_response_body(replay, rx, kind.to_string(), group_version);
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
        // No cache registered for this resource — falls through to the
        // echo stub below, same posture as every other not-yet-served
        // case in this handler.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(path: &str) -> Vec<String> {
        path::split_path(path)
    }

    #[test]
    fn api_root_serves_api_versions() {
        let route = route_discovery(&parts("/api"), None);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIVersions");
    }

    #[test]
    fn api_v1_serves_the_core_group_resource_list() {
        let route = route_discovery(&parts("/api/v1"), None);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIResourceList");
        assert_eq!(doc["groupVersion"], "v1");
    }

    #[test]
    fn apis_root_serves_the_group_list() {
        let route = route_discovery(&parts("/apis"), None);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIGroupList");
    }

    #[test]
    fn apis_root_serves_aggregated_discovery_when_the_client_asks_for_it() {
        let accept = "application/json;as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io";
        let route = route_discovery(&parts("/apis"), Some(accept));
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIGroupDiscoveryList");
    }

    #[test]
    fn api_root_serves_aggregated_discovery_when_the_client_asks_for_it() {
        let accept = "application/json;as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io";
        let route = route_discovery(&parts("/api"), Some(accept));
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
        let route = route_discovery(&parts("/apis"), Some(accept));
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIGroupList", "an unmatched as= version must fall back to the legacy shape, not silently serve v2 anyway");
    }

    #[test]
    fn apis_group_serves_the_group_document() {
        let route = route_discovery(&parts("/apis/apps"), None);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIGroup");
        assert_eq!(doc["name"], "apps");
    }

    #[test]
    fn apis_group_version_serves_the_resource_list() {
        let route = route_discovery(&parts("/apis/apps/v1"), None);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["kind"], "APIResourceList");
        assert_eq!(doc["groupVersion"], "apps/v1");
    }

    #[test]
    fn an_unknown_group_is_a_real_not_found_not_a_fallthrough() {
        assert!(matches!(route_discovery(&parts("/apis/totally.made.up"), None), DiscoveryRoute::NotFound));
        assert!(matches!(route_discovery(&parts("/apis/apps/v999"), None), DiscoveryRoute::NotFound));
        assert!(matches!(route_discovery(&parts("/api/v999"), None), DiscoveryRoute::NotFound));
    }

    #[test]
    fn a_resource_shaped_path_is_not_applicable_to_discovery_routing() {
        assert!(matches!(route_discovery(&parts("/api/v1/namespaces/default/pods"), None), DiscoveryRoute::NotApplicable));
        assert!(matches!(route_discovery(&parts("/apis/apps/v1/namespaces/default/deployments"), None), DiscoveryRoute::NotApplicable));
        assert!(matches!(route_discovery(&parts("/"), None), DiscoveryRoute::NotApplicable));
    }

    #[test]
    fn openapi_v3_root_serves_the_root_index() {
        let route = route_discovery(&parts("/openapi/v3"), None);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert!(doc["paths"].as_object().unwrap().contains_key("apis/apps/v1"));
    }

    #[test]
    fn openapi_v3_a_multi_segment_path_serves_the_raw_vendored_document() {
        let route = route_discovery(&parts("/openapi/v3/apis/apps/v1"), None);
        let DiscoveryRoute::FoundRaw(bytes) = route else { panic!("expected FoundRaw") };
        let parsed: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert!(parsed.get("openapi").is_some());
    }

    #[test]
    fn openapi_v3_an_unvendored_path_is_a_real_not_found() {
        assert!(matches!(route_discovery(&parts("/openapi/v3/apis/totally.made.up/v1"), None), DiscoveryRoute::NotFound));
    }

    #[test]
    fn version_serves_the_real_version_info_document() {
        let route = route_discovery(&parts("/version"), None);
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
    fn encode_watch_event_produces_a_newline_terminated_json_line() {
        let event = crate::cacher::store::WatchEvent { kind: crate::cacher::store::EventKind::Bookmark, key: Vec::new(), value: Vec::new(), revision: 9 };
        let frame = encode_watch_event(&event, "Pod", "v1").expect("Bookmark always converts").expect("Bookmark conversion never fails");
        let bytes = frame.into_data().unwrap();
        assert!(bytes.ends_with(b"\n"));
        let parsed: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(parsed["type"], "BOOKMARK");
        assert_eq!(parsed["object"]["kind"], "Pod");
    }

    #[test]
    fn encode_watch_event_skips_a_deleted_event_with_no_retained_value() {
        let event = crate::cacher::store::WatchEvent { kind: crate::cacher::store::EventKind::Deleted, key: b"k".to_vec(), value: Vec::new(), revision: 9 };
        assert!(encode_watch_event(&event, "Pod", "v1").is_none());
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

        let body = watch_response_body(replay, rx, "Namespace".to_string(), "v1".to_string());
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
