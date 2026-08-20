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
//! `cacher::registry::CacheRegistry` cache for one proof-of-concept
//! resource (`namespaces`) and `GET` consults it when the request
//! targets that exact resource (`rest::get`'s own `Option<&SharedCache>`
//! parameter) — every other resource still reads straight from
//! nodestore, and this is one concrete case, not a general policy (see
//! `cacher::registry`'s own doc comment for why enumerating every
//! resource at boot isn't done yet). **Every other verb is still a
//! bring-up stub** — a `watch`/`patch`/`deletecollection` against
//! `/api(s)/.../<resource>` still just echoes the parsed
//! [`crate::server::path::RequestInfo`] as JSON, not the real REST
//! dispatch. Client certificate authentication is real
//! (`super::tls`'s optional `client_ca`, `authn::x509::identity_from_der`
//! on the verified peer cert), surfaced in the echo response's own `user`
//! field for observability. Authorization is real too, but **opt-in and
//! off by default** (`config::Config::enforce_rbac`/`NODEAPISERVER_ENFORCE_RBAC`
//! — see that field's own doc comment for why: enabling RBAC enforcement
//! before Group O's bootstrap `ClusterRole`/`ClusterRoleBinding` set
//! exists can lock every request out with no path back in). When
//! enabled, `GET`/`LIST`/`CREATE`/`DELETE`/`UPDATE` all resolve the caller's real rules
//! (`authz::resolve::rules_for` — the real anonymous identity,
//! `system:anonymous`/`system:unauthenticated`, when no x509 identity was
//! established) and deny with a real `403` unless `authz::rbac::rules_allow`
//! says yes. When disabled (the default), every request is still served
//! the same way regardless of identity, same as before this existed. The
//! real handler chain (authentication -> authorization ->
//! priority-and-fairness -> admission -> REST, `docs/APISERVER.md`'s own
//! hard requirement) replaces all of this once admission (Group J) exists
//! too.
//!
//! What *is* real now: `/healthz`, and every non-resource discovery route
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

use crate::authz;
use crate::config::Config;
use crate::codec::negotiation;
use crate::server::{discovery, openapi, path, rest, version};
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

    // Group D: a real, working proof of concept for the cache
    // consultation `rest::get` gained its `Option<&SharedCache>`
    // parameter for — one resource, `namespaces` (the same one Group
    // F's first verified name-format rule already targets), so this
    // wiring is actually observable rather than dead code with every
    // call site still passing `None`. Real per-resource cache
    // registration policy (which resources, how many at once, whether
    // to wait for initial sync before serving traffic) is still not
    // decided for anything beyond this one concrete case — see
    // `cacher::registry`'s own doc comment. `StorageClient::clone()` is
    // cheap (a `tonic::transport::Channel` clone), so this doesn't cost
    // a second real connection.
    let namespaces_cache = storage.as_ref().map(|s| {
        let registry = crate::cacher::CacheRegistry::new();
        registry.spawn(s.clone(), "", "v1", "namespaces")
    });

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
    info!(%addr, storage_connected = storage.is_some(), enforce_rbac = cfg.enforce_rbac, namespaces_cache = namespaces_cache.is_some(), "nodeapiserver: REST/watch listener up (discovery + GET/LIST/CREATE/DELETE/UPDATE are real; every other resource verb is still a bring-up stub — see server::listener's own doc comment)");
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
        let namespaces_cache = namespaces_cache.clone();
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
            let service = hyper::service::service_fn(move |req| handle(req, storage.clone(), namespaces_cache.clone(), identity.clone(), enforce_rbac));
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

async fn handle(req: Request<Incoming>, storage: Option<StorageClient>, namespaces_cache: Option<crate::cacher::store::SharedCache>, identity: Option<crate::authn::x509::Identity>, enforce_rbac: bool) -> Result<Response<BoxedBody>, Infallible> {
    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();

    if path_str == "/healthz" {
        return Ok(Response::builder().status(StatusCode::OK).header("Content-Type", "text/plain").body(body_from_bytes(b"ok".to_vec())).unwrap());
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
            let body_value = if has_body {
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

            // Only `namespaces` (core group) has a real cache registered at
            // all today (`run()`'s own proof-of-concept `namespaces_cache`)
            // — every other resource still passes `None` to both `get` and
            // `list`, same as before this cache existed. Shared by both
            // verbs below; `rest::list`'s own doc comment covers why an
            // unsynced cache is safe to pass here too (it just falls
            // through, same as `None`).
            let resource_cache = if info.api_group.is_empty() && info.resource == "namespaces" { namespaces_cache.as_ref() } else { None };

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
}
