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
//! `cacher::registry::CacheRegistry` cache for every built-in resource in
//! the generated discovery table; CRD-defined resources are registered
//! from the live CustomResourceDefinition cache after their Established
//! CRD is discovered. `GET`/`LIST`
//! consult one whenever the request targets a registered resource.
//! `WATCH` against a resource with a
//! registered cache is real too now (`is_watch`'s own doc comment, and
//! `watch_response_body`): the cache's own retained history replays
//! first, then live events stream as they happen, real `Transfer-Encoding`
//! framing handled by hyper's own h1/h2 connection layer. A resource with
//! no registered cache returns a real Kubernetes error instead of a false
//! success, same as an unsupported resource verb. `PATCH` is real
//! too now (`rest::patch_prepare`/`patch_persist`, reusing Group G's
//! already-landed `patch::json_patch`/`merge_patch`/`strategic_merge`,
//! selected by the real `Content-Type` —
//! `application/json-patch+json`/`application/merge-patch+json`/
//! `application/strategic-merge-patch+json` — with Kubernetes' default
//! strategic-merge/CRD merge-patch selection when `Content-Type` is
//! omitted and a real `415` for an unsupported explicit type). PATCH runs
//! the shared pure-mutator registry and the storage-backed Group J stages
//! that apply to an `Update`-shaped write (`namespace_lifecycle`,
//! `LimitRanger`'s own PVC validation, and the PVC resize check); the
//! `rest::patch_prepare`/`patch_persist` split exists specifically so
//! admission can see the real candidate object in between the two).
//! `deletecollection`
//! is real too now (it lists via the same selector filtering `LIST` already
//! has, runs configured admission against each matched object, then deletes
//! each match) — `watch` is the only remaining resource verb
//! this build knows about that isn't a real generic REST dispatch — it's
//! real too, just structurally different (a streaming response, covered
//! above).
//! Client certificate authentication is real (`super::tls`'s optional
//! `client_ca`, `authn::x509::identity_from_der` on the verified peer
//! cert), used for authentication and audit identity. Authorization is real
//! too, but **opt-in and off by
//! default** (`config::Config::enforce_rbac`/`NODEAPISERVER_ENFORCE_RBAC`
//! — see that field's own doc comment for why: enabling RBAC enforcement
//! before Group O's bootstrap `ClusterRole`/`ClusterRoleBinding` set
//! exists can lock every request out with no path back in). When
//! enabled, `GET`/`LIST`/`CREATE`/`DELETE`/`UPDATE`/`WATCH` all resolve
//! the caller's real rules (`authz::resolve::rules_for` — the real
//! anonymous identity, `system:anonymous`/`system:unauthenticated`, when
//! no x509 identity was established) and deny with a real `403` unless
//! `authz::rbac::rules_allow` says yes. Group J admission (eight
//! unconditional plugins as of this revision, `admission`'s own doc
//! comment has the running list) also runs, on the real write verbs only
//! — real upstream's own admission posture too, admission never gates a
//! read. The real handler chain (authentication -> authorization ->
//! priority-and-fairness -> admission -> REST, `docs/APISERVER.md`'s own
//! hard requirement) is still not fully unified into one ordered
//! pipeline — pure mutators now share one dispatcher across all candidate
//! write paths, while storage-backed stages retain explicit request-specific
//! handling for their I/O and failure policies.
//!
//! What *is* real now: `/healthz`/`/readyz`/`/livez` (`server::healthz` —
//! real upstream's own per-check response shape, `?verbose` included; see
//! that module's own doc comment for exactly which checks are ported),
//! and every non-resource discovery route
//! (`/api`, `/api/{version}`, `/apis`, `/apis/{group}`,
//! `/apis/{group}/{version}`, `/openapi/v2`, `/openapi/v3(/...)`, `/version`) is answered
//! by `server::discovery`/`openapi`/`version`'s real document builders
//! (`route_discovery`, pure and unit-tested below) rather than falling
//! into a false-success response — these are the routes `kubectl` itself calls
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
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BoxedBody = http_body_util::combinators::BoxBody<hyper::body::Bytes, BoxError>;

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
            Self::TooLarge { limit } => write!(formatter, "request body exceeds the {limit}-byte limit"),
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
            return (Response::from_parts(parts, body_from_bytes(Vec::new())), None);
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

fn json_response_with_content_type(status: StatusCode, value: &serde_json::Value, content_type: &str) -> Response<BoxedBody> {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder().status(status).header("Content-Type", content_type).body(body_from_bytes(bytes)).unwrap()
}

fn scale_outcome_response(path: &str, outcome: rest::ScaleOutcome) -> Response<BoxedBody> {
    match outcome {
        rest::ScaleOutcome::Found(scale) | rest::ScaleOutcome::Updated(scale) => json_response(StatusCode::OK, &scale),
        rest::ScaleOutcome::UnknownResource | rest::ScaleOutcome::ObjectNotFound => json_response(StatusCode::NOT_FOUND, &not_found_status(path)),
        rest::ScaleOutcome::MissingResourceVersion => json_response(StatusCode::BAD_REQUEST, &bad_request_status(path, "metadata.resourceVersion is required")),
        rest::ScaleOutcome::Conflict => json_response(StatusCode::CONFLICT, &conflict_status(path)),
        rest::ScaleOutcome::Invalid(violations) => json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(path, &violations)),
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
    initial_events_end: bool,
) -> Option<Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>> {
    match crate::server::watch_event::to_watch_event_json(event, kind, api_version, storage, group, resource) {
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
            Some(Ok(hyper::body::Frame::data(hyper::body::Bytes::from(bytes))))
        }
        Some(Err(error)) => Some(Err(Box::new(error) as BoxError)),
    }
}

fn mark_initial_events_end(event_json: &mut Value) {
    let Some(object) = event_json.get_mut("object").and_then(Value::as_object_mut) else {
        return;
    };
    let metadata = object.entry("metadata").or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !metadata.is_object() {
        *metadata = Value::Object(serde_json::Map::new());
    }
    let Some(metadata) = metadata.as_object_mut() else {
        return;
    };
    let annotations = metadata.entry("annotations").or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !annotations.is_object() {
        *annotations = Value::Object(serde_json::Map::new());
    }
    annotations["k8s.io/initial-events-end"] = Value::String("true".to_string());
}

type WatchStreamEvent = (crate::cacher::store::WatchEvent, bool);
type WatchEventStream = Pin<Box<dyn tokio_stream::Stream<Item = WatchStreamEvent> + Send + Sync>>;
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
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt;

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
            allow_watch_bookmarks || *initial_events_end || event.kind != crate::cacher::store::EventKind::Bookmark
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
    let (storage_for_filter, group_for_filter, resource_for_filter) = (storage.clone(), group.clone(), resource.clone());
    let filtered = events.filter(move |(event, _)| watch_event_matches_selector(event, &label_reqs, &field_reqs, storage_for_filter.as_ref(), &group_for_filter, &resource_for_filter));
    if conversion_webhook.is_none() {
        let frames = filtered.filter_map(move |(event, initial_events_end)| {
            encode_watch_event(&event, &kind, &api_version, storage.as_ref(), &group, &resource, &version, partial_metadata, initial_events_end)
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
        Ok(c) => Arc::new(c),
        Err(e) => {
            warn!(error = ?e, "failed to load/generate the TLS certificate; the REST/watch listener will not run");
            return;
        }
    };

    // Group H: client certificate authentication is offered but not
    // required (see server::tls's own doc comment). The CA bundle is
    // reloadable, so a valid replacement applies to new connections without
    // restarting the listener. A misconfigured initial file still disables
    // client-cert auth for this run rather than stopping the listener.
    let client_ca = match &cfg.client_ca_file {
        Some(path) => match super::tls::ReloadableClientCa::from_file(path) {
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
        Some(path) => match crate::authn::service_account::ReloadableAuthenticator::from_pem(path, cfg.service_account_issuer.clone()) {
            Ok(authenticator) => Some(Arc::new(authenticator)),
            Err(e) => {
                warn!(path = %path.display(), error = ?e, "failed to load NODEAPISERVER_SERVICE_ACCOUNT_SIGNING_KEY_FILE; the REST/watch listener will not run");
                return;
            }
        },
        None => None,
    };

    // Group H: the upstream-compatible static token file is optional. A
    // malformed initial file disables the listener rather than leaving a
    // partially loaded token table in place; later malformed rotations are
    // handled by ReloadableAuthenticator, which retains the last valid table.
    let bootstrap_token_authenticator = match &cfg.bootstrap_token_file {
        Some(path) => match crate::authn::bootstrap_token::ReloadableAuthenticator::from_file(path) {
            Ok(authenticator) => Some(Arc::new(authenticator)),
            Err(error) => {
                warn!(path = %path.display(), error, "failed to load NODEAPISERVER_TOKEN_AUTH_FILE; the REST/watch listener will not run");
                return;
            }
        },
        None => None,
    };

    // Group H: OIDC is optional, but a configured issuer must complete
    // discovery and load a usable JWKS before its bearer tokens are accepted.
    // If that setup fails, keep OIDC disabled rather than accepting tokens
    // without a verified identity.
    let oidc_authenticator = match (&cfg.oidc_issuer_url, &cfg.oidc_client_id) {
        (Some(issuer_url), Some(client_id)) => {
            let ca_certificate_pem = match &cfg.oidc_ca_file {
                Some(path) => match std::fs::read(path) {
                    Ok(bytes) => Some(bytes),
                    Err(error) => {
                        warn!(path = %path.display(), error = ?error, "failed to read NODEAPISERVER_OIDC_CA_FILE; OIDC authentication is disabled for this run");
                        None
                    }
                },
                None => None,
            };
            if cfg.oidc_ca_file.is_some() && ca_certificate_pem.is_none() {
                None
            } else {
                let oidc_config = crate::authn::oidc::Config {
                    issuer_url: issuer_url.clone(),
                    client_id: client_id.clone(),
                    username_claim: cfg.oidc_username_claim.clone(),
                    username_prefix: cfg.oidc_username_prefix.clone(),
                    groups_claim: cfg.oidc_groups_claim.clone(),
                    groups_prefix: cfg.oidc_groups_prefix.clone(),
                    required_claims: cfg.oidc_required_claims.clone(),
                    signing_algs: cfg.oidc_signing_algs.clone(),
                    ca_certificate_pem,
                };
                match crate::authn::oidc::Authenticator::from_config(oidc_config).await {
                    Ok(authenticator) => Some(Arc::new(authenticator)),
                    Err(error) => {
                        warn!(issuer = %issuer_url, error = ?error, "OIDC discovery/JWKS initialization failed; OIDC authentication is disabled for this run");
                        None
                    }
                }
            }
        }
        _ => None,
    };

    let authorization_webhook = match (
        cfg.authorization_webhook_url.as_deref(),
        cfg.authorization_webhook_config_file.as_deref(),
    ) {
        (Some(url), None) => match crate::authz::webhook::WebhookAuthorizer::new_with_cache_ttls(
            url.to_string(),
            cfg.authorization_webhook_authorized_ttl,
            cfg.authorization_webhook_unauthorized_ttl,
        ) {
            Ok(authorizer) => Some(Arc::new(authorizer)),
            Err(error) => {
                warn!(%url, error = ?error, "invalid NODEAPISERVER_AUTHORIZATION_WEBHOOK_URL; the REST/watch listener will not run");
                return;
            }
        },
        (None, Some(path)) => match crate::authz::webhook::WebhookAuthorizer::from_kubeconfig(
            path,
            cfg.authorization_webhook_authorized_ttl,
            cfg.authorization_webhook_unauthorized_ttl,
        ) {
            Ok(authorizer) => {
                info!(path = %path.display(), "nodeapiserver: configured authorization webhook from kubeconfig");
                Some(Arc::new(authorizer))
            }
            Err(error) => {
                warn!(path = %path.display(), error = ?error, "failed to load NODEAPISERVER_AUTHORIZATION_WEBHOOK_CONFIG_FILE; the REST/watch listener will not run");
                return;
            }
        },
        (None, None) => None,
        (Some(_), Some(_)) => {
            warn!("authorization webhook URL and config file are mutually exclusive; the REST/watch listener will not run");
            return;
        }
    };

    // Group L: aggregated API servers may use the request-header authenticator
    // and therefore require the apiserver's trusted front-proxy client
    // certificate. Load it once here; the per-APIService CA bundle still
    // controls the backend serving certificate independently.
    let aggregation_proxy_identity = match (&cfg.proxy_client_cert_file, &cfg.proxy_client_key_file) {
        (Some(cert), Some(key)) => match crate::aggregator::client_tls::ClientIdentity::from_files(cert, key) {
            Ok(identity) => Some(Arc::new(identity)),
            Err(error) => {
                warn!(cert = %cert.display(), key = %key.display(), error = ?error, "failed to load the aggregation proxy client identity; the REST/watch listener will not run");
                return;
            }
        },
        _ => None,
    };

    let audit_webhook = match (cfg.audit_webhook_url.as_deref(), cfg.audit_webhook_config_file.as_deref()) {
        (Some(url), None) => match crate::audit::webhook::AuditWebhook::new(url) {
            Ok(webhook) => {
                info!(%url, "nodeapiserver: configured audit webhook");
                Some(webhook)
            }
            Err(error) => {
                warn!(%url, error, "invalid NODEAPISERVER_AUDIT_WEBHOOK_URL; the REST/watch listener will not run");
                return;
            }
        },
        (None, Some(path)) => match crate::audit::webhook::AuditWebhook::from_kubeconfig(path) {
            Ok(webhook) => {
                info!(path = %path.display(), "nodeapiserver: configured audit webhook from kubeconfig");
                Some(webhook)
            }
            Err(error) => {
                warn!(path = %path.display(), error, "failed to load NODEAPISERVER_AUDIT_WEBHOOK_CONFIG_FILE; the REST/watch listener will not run");
                return;
            }
        },
        (None, None) => None,
        (Some(_), Some(_)) => {
            warn!("audit webhook URL and config file are mutually exclusive; the REST/watch listener will not run");
            return;
        }
    };
    let audit_sink = match cfg.audit_log_path.as_deref() {
        Some(path) => match crate::audit::sink::AuditSink::open_with_rotation(
            path,
            cfg.audit_log_max_size_bytes,
            cfg.audit_log_max_backups,
        ) {
            Ok(sink) => {
                info!(path = %path.display(), "nodeapiserver: opened audit log");
                Some(Arc::new(match audit_webhook {
                    Some(webhook) => sink.with_webhook(webhook),
                    None => sink,
                }))
            }
            Err(error) => {
                warn!(path = %path.display(), error = ?error, "failed to open NODEAPISERVER_AUDIT_LOG_PATH; the REST/watch listener will not run");
                return;
            }
        },
        None => audit_webhook.map(|webhook| Arc::new(crate::audit::sink::AuditSink::webhook_only(webhook))),
    };
    let audit_policy = match cfg.audit_policy_file.as_deref() {
        Some(path) => match crate::audit::policy::AuditPolicy::from_file(path) {
            Ok(policy) => {
                info!(path = %path.display(), "nodeapiserver: loaded audit policy");
                Some(Arc::new(policy))
            }
            Err(error) => {
                warn!(path = %path.display(), error, "failed to load NODEAPISERVER_AUDIT_POLICY_FILE; the REST/watch listener will not run");
                return;
            }
        },
        None => None,
    };

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
    // resource requests return a real 503 when this is `None` (see the
    // request handler's own call-site guard). Connected once here and cloned
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
            warn!(error = ?e, "failed to connect to nodestore at startup; resource requests will return 503 until this succeeds");
            None
        }
    };

    // Group D: register one reflector for every built-in resource in the
    // generated discovery table. `StorageClient::clone()` is cheap (a
    // `tonic::transport::Channel` clone), and each reflector shares the
    // same nodestore connection pool while keeping one cache per GVR, like
    // a real informer factory.
    let cache_registry = crate::cacher::CacheRegistry::new();
    if let Some(s) = storage.as_ref() {
        for resource in crate::codegen::api_resources::API_RESOURCES {
            cache_registry.spawn(s.clone(), resource.group, resource.version, resource.resource);
        }

        // Group K: CRD-backed caches follow the CRD watch rather than
        // waiting for a client to issue the first watch against each new
        // resource. This also retires reflectors when a CRD is removed or
        // stops serving a version, so a deleted definition cannot leave a
        // stale resource cache alive in this process.
        if let Some(crd_cache) = cache_registry.get("apiextensions.k8s.io", "v1", "customresourcedefinitions") {
            let crd_storage = s.clone();
            let crd_registry = cache_registry.clone();
            tokio::spawn(async move {
                reconcile_crd_caches(crd_storage, crd_cache, crd_registry).await;
            });
        } else {
            warn!("crd cache: built-in CustomResourceDefinition cache was not registered");
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
    info!(%addr, storage_connected = storage.is_some(), enforce_rbac = cfg.enforce_rbac, anonymous_auth = cfg.anonymous_auth, max_request_body_bytes = cfg.max_request_body_bytes, cached_resources = crate::codegen::api_resources::API_RESOURCES.len(), "nodeapiserver: REST/watch listener up (discovery + GET/LIST/CREATE/DELETE/UPDATE/PATCH/DELETECOLLECTION/WATCH are real; unsupported paths return Kubernetes errors — see server::listener's own doc comment)");
    let enforce_rbac = cfg.enforce_rbac;
    let concurrency_limiter = Arc::new(crate::flowcontrol::limiter::ConcurrencyLimiter::new(
        cfg.apf_max_requests_inflight,
        cfg.apf_max_mutating_requests_inflight,
        cfg.apf_queue_length_limit,
    ));
    let anonymous_auth = cfg.anonymous_auth;
    let max_request_body_bytes = cfg.max_request_body_bytes;
    // Pure admission plugins are immutable after startup. Keep one ordered
    // dispatcher for all connections instead of rebuilding its trait-object
    // chain for every write request; storage-backed plugins remain in the
    // request path because they require their own I/O and failure policy.
    let pure_admission = Arc::new(crate::admission::chain::MutatingRegistry::with_builtins());

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
        let cert = cert.clone();
        let client_ca = client_ca.clone();
        let storage = storage.clone();
        let cache_registry = cache_registry.clone();
        let pure_admission = pure_admission.clone();
        let kubelet_tls = kubelet_tls.clone();
        let service_account_authenticator = service_account_authenticator.clone();
        let oidc_authenticator = oidc_authenticator.clone();
        let bootstrap_token_authenticator = bootstrap_token_authenticator.clone();
        let authorization_webhook = authorization_webhook.clone();
        let aggregation_proxy_identity = aggregation_proxy_identity.clone();
        let concurrency_limiter = concurrency_limiter.clone();
        let audit_sink = audit_sink.clone();
        let audit_policy = audit_policy.clone();
        let max_request_body_bytes = max_request_body_bytes;
        tokio::spawn(async move {
            let client_ca_store = client_ca.as_ref().map(super::tls::ReloadableClientCa::current);
            let server_config = match cert.server_config(client_ca_store.as_ref()) {
                Ok(config) => config,
                Err(error) => {
                    warn!(%peer, error = ?error, "listener: failed to build the TLS server config for the connection");
                    return;
                }
            };
            let acceptor = TlsAcceptor::from(Arc::new(server_config));
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
            let service = hyper::service::service_fn(move |req| handle_with_audit(req, storage.clone(), cache_registry.clone(), pure_admission.clone(), identity.clone(), bootstrap_token_authenticator.clone(), service_account_authenticator.clone(), oidc_authenticator.clone(), authorization_webhook.clone(), aggregation_proxy_identity.clone(), concurrency_limiter.clone(), audit_sink.clone(), audit_policy.clone(), anonymous_auth, enforce_rbac, max_request_body_bytes, peer, kubelet_tls.clone()));
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
/// silent fallthrough into the resource-request handler, which would
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

const AGGREGATED_DISCOVERY_CONTENT_TYPE: &str = "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList";

fn discovery_content_type(parts: &[String], accept_header: Option<&str>) -> &'static str {
    if parts.len() == 1 && matches!(parts.first().map(String::as_str), Some("api") | Some("apis")) && wants_aggregated_discovery(accept_header) {
        AGGREGATED_DISCOVERY_CONTENT_TYPE
    } else {
        "application/json"
    }
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
        (Some("openapi"), Some("v2"), 2) => DiscoveryRoute::Found(openapi::v2()),
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

fn request_entity_too_large_status(path_str: &str, limit: usize) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: request body exceeds the {limit}-byte limit"),
        "reason": "RequestEntityTooLarge",
        "details": {},
        "code": 413,
    })
}

/// Same minimal `Status` shape again, for an RBAC denial (`enforce_rbac`
/// only — see this module's own doc comment) — real upstream's
/// `reason: "Forbidden"`, `code: 403`.
fn forbidden_status(path_str: &str, user_name: &str) -> serde_json::Value {
    forbidden_status_with_reason(path_str, user_name, "")
}

fn forbidden_status_with_reason(path_str: &str, user_name: &str, reason: &str) -> serde_json::Value {
    let message = if reason.is_empty() {
        format!("{path_str}: User {user_name:?} does not have permission for this request (RBAC)")
    } else {
        format!("{path_str}: {reason}")
    };
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": message,
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

fn admission_webhook_error_response(
    path_str: &str,
    error: &admission::webhook::Error,
) -> Response<BoxedBody> {
    match error {
        admission::webhook::Error::DryRunUnsupported { detail, .. } => {
            json_response(StatusCode::BAD_REQUEST, &bad_request_status(path_str, detail))
        }
        _ => json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str)),
    }
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

fn too_many_requests_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: the API request queue is full"),
        "reason": "TooManyRequests",
        "details": {},
        "code": 429,
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

fn precondition_failed_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: delete precondition failed"),
        "reason": "Conflict",
        "details": {},
        "code": 409,
    })
}

/// Real upstream's own `Invalid` shape for a write that failed validation —
/// `reason: "Invalid"`, `code: 422`. Keep both the human-readable aggregate
/// message and one `StatusCause` per violation: kubectl and controller
/// clients use `details.causes[].field` to point at the invalid field rather
/// than parsing the aggregate message.
fn invalid_status(path_str: &str, violations: &[String]) -> serde_json::Value {
    let causes: Vec<serde_json::Value> = violations
        .iter()
        .map(|violation| {
            let (field, message) = violation
                .split_once(": ")
                .map_or(("", violation.as_str()), |(field, message)| (field, message));
            let reason = if message == "Required value" {
                "FieldValueRequired"
            } else {
                "FieldValueInvalid"
            };
            serde_json::json!({
                "reason": reason,
                "message": message,
                "field": field,
            })
        })
        .collect();
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str} is invalid: {}", violations.join("; ")),
        "reason": "Invalid",
        "details": {"causes": causes},
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

            match rest::update_status(client, "", "v1", "resourcequotas", Some(namespace), &quota_name, &status_body, false).await {
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
/// call, logged rather than delegated back into `handle` itself. The
/// wrapper keeps the audit context at the request boundary and explicitly
/// records responses that finish before `handle` runs, while the normal
/// response path is audited after `handle` returns. The sink is this crate's
/// own `tracing` output (`target: "nodeapiserver::audit"`, one JSON line per
/// request) and, when configured, an append-only file selected by
/// `NODEAPISERVER_AUDIT_LOG_PATH` or a bounded asynchronous webhook selected
/// by `NODEAPISERVER_AUDIT_WEBHOOK_URL`. See
/// `audit::event`'s own doc comment for exactly which real `Event`
/// fields are populated and which stages/levels this uses.
async fn handle_with_audit(
    req: Request<Incoming>,
    storage: Option<StorageClient>,
    cache_registry: crate::cacher::CacheRegistry,
    pure_admission: Arc<crate::admission::chain::MutatingRegistry>,
    identity: Option<crate::authn::x509::Identity>,
    bootstrap_token_authenticator: Option<Arc<crate::authn::bootstrap_token::ReloadableAuthenticator>>,
    service_account_authenticator: Option<Arc<crate::authn::service_account::ReloadableAuthenticator>>,
    oidc_authenticator: Option<Arc<crate::authn::oidc::Authenticator>>,
    authorization_webhook: Option<Arc<crate::authz::webhook::WebhookAuthorizer>>,
    aggregation_proxy_identity: Option<Arc<crate::aggregator::client_tls::ClientIdentity>>,
    concurrency_limiter: Arc<crate::flowcontrol::limiter::ConcurrencyLimiter>,
    audit_sink: Option<Arc<crate::audit::sink::AuditSink>>,
    audit_policy: Option<Arc<crate::audit::policy::AuditPolicy>>,
    anonymous_auth: bool,
    enforce_rbac: bool,
    max_request_body_bytes: usize,
    peer: SocketAddr,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Result<Response<BoxedBody>, Infallible> {
    let admission_metadata = Arc::new(Mutex::new(AdmissionMetadata::default()));
    let mut req = req;
    req.extensions_mut().insert(admission_metadata.clone());
    req.extensions_mut().insert(RequestBodyLimit(max_request_body_bytes));
    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let user_agent = req.headers().get("user-agent").and_then(|v| v.to_str().ok()).map(str::to_string);
    let request_info = path::parse(&method, &path_str, &query);
    let audit_id = uuid::Uuid::new_v4().to_string();
    let identity = match authenticate_request(
        &req,
        identity,
        bootstrap_token_authenticator.as_deref(),
        service_account_authenticator.as_deref(),
        oidc_authenticator.as_deref(),
        anonymous_auth,
    )
    .await
    {
        Ok(identity) => identity,
        Err(detail) => {
            let response = json_response(
                StatusCode::UNAUTHORIZED,
                &unauthorized_status(&path_str, detail),
            );
            log_audit_rejected_request(
                &audit_id,
                &request_info,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                None,
                &peer,
                response.status().as_u16(),
                audit_sink.as_deref(),
                audit_policy.as_deref(),
            );
            return Ok(response);
        }
    };
    let audit_identity = identity.clone();
    let audit_user = audit_identity.as_ref().map(|identity| identity.name.as_str()).unwrap_or(ANONYMOUS_USERNAME);
    let audit_groups = audit_identity
        .as_ref()
        .map(|identity| identity.groups.clone())
        .unwrap_or_else(|| vec![UNAUTHENTICATED_GROUP.to_string()]);
    let long_running = is_long_running_request(&request_info, &query);
    let audit_level = audit_policy
        .as_ref()
        .map(|policy| policy.decide(&request_info, audit_user, &audit_groups).level)
        .unwrap_or(crate::audit::policy::Level::Metadata);
    let capture_request_body = !long_running
        && matches!(
            audit_level,
            crate::audit::policy::Level::Request | crate::audit::policy::Level::RequestResponse
        );
    let request_body_capture = capture_request_body.then(|| {
        let capture = Arc::new(Mutex::new(None));
        req.extensions_mut().insert(AuditRequestBodyCapture(capture.clone()));
        capture
    });
    let request_content_type = req
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let audit_request_received = audit_policy.as_ref().is_some_and(|policy| {
        policy.should_emit_stage(
            &request_info,
            audit_user,
            &audit_groups,
            crate::audit::event::STAGE_REQUEST_RECEIVED,
        )
    });
    let audit_response_started = long_running
        && audit_policy.as_ref().map_or(true, |policy| {
            policy.should_emit_stage(
                &request_info,
                audit_user,
                &audit_groups,
                crate::audit::event::STAGE_RESPONSE_STARTED,
            )
        });
    let audit_response_complete = !long_running
        && audit_policy.as_ref().map_or(true, |policy| {
            policy.should_emit_stage(
                &request_info,
                audit_user,
                &audit_groups,
                crate::audit::event::STAGE_RESPONSE_COMPLETE,
            )
        });
    if audit_request_received && !capture_request_body {
        log_audit_event(
            &audit_id,
            crate::audit::event::STAGE_REQUEST_RECEIVED,
            &method,
            &path_str,
            &query,
            user_agent.as_deref(),
            audit_identity.as_ref(),
            &peer,
            0,
            audit_sink.as_deref(),
            &BTreeMap::new(),
        );
    }
    let mut authorization_webhook_allowed = false;
    if let Some(authorizer) = authorization_webhook {
        match authorizer
            .authorize_with_details(&request_info, identity.as_ref())
            .await
        {
            Ok(details) => {
                if let Some(error) = details.evaluation_error.as_deref() {
                    warn!(path = %path_str, evaluation_error = %error, "authorization webhook returned an evaluation error");
                }
                match details.decision {
                    crate::authz::webhook::Decision::Allow => {
                        authorization_webhook_allowed = true;
                    }
                    crate::authz::webhook::Decision::NoOpinion => {}
                    crate::authz::webhook::Decision::Deny => {
                        let user_name = identity
                            .as_ref()
                            .map(|identity| identity.name.as_str())
                            .unwrap_or(ANONYMOUS_USERNAME);
                        let response = json_response(
                            StatusCode::FORBIDDEN,
                            &forbidden_status_with_reason(
                                &path_str,
                                user_name,
                                &details.reason,
                            ),
                        );
                        log_audit_rejected_request(
                            &audit_id,
                            &request_info,
                            &method,
                            &path_str,
                            &query,
                            user_agent.as_deref(),
                            identity.as_ref(),
                            &peer,
                            response.status().as_u16(),
                            audit_sink.as_deref(),
                            audit_policy.as_deref(),
                        );
                        return Ok(response);
                    }
                }
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "authorization webhook failed");
                let response = json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &service_unavailable_status(&path_str, "authorization webhook unavailable"),
                );
                log_audit_rejected_request(
                    &audit_id,
                    &request_info,
                    &method,
                    &path_str,
                    &query,
                    user_agent.as_deref(),
                    identity.as_ref(),
                    &peer,
                    response.status().as_u16(),
                    audit_sink.as_deref(),
                    audit_policy.as_deref(),
                );
                return Ok(response);
            }
        }
    }
    let selected_priority = if let Some(mut client) = storage.clone() {
        let (user_name, user_groups): (&str, Vec<String>) = match identity.as_ref() {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let digest = flowcontrol::flow_schema::RequestDigest {
            user_name,
            user_groups: &user_groups,
            verb: &request_info.verb,
            is_resource_request: request_info.is_resource_request,
            api_group: &request_info.api_group,
            resource: &request_info.resource,
            subresource: &request_info.subresource,
            namespace: &request_info.namespace,
            path: &request_info.path,
        };
        flowcontrol::resolve::select_for_request(&mut client, &digest).await
    } else {
        None
    };
    let selected_priority_config = selected_priority.as_ref().map(|selected| &selected.priority_level);
    let configured_priorities = selected_priority
        .as_ref()
        .map(|selected| selected.priority_levels.as_slice())
        .unwrap_or(&[]);
    let flow_distinguisher = selected_priority.as_ref().map(|selected| selected.flow_distinguisher.as_str()).unwrap_or("");
    let _permit = match concurrency_limiter
        .acquire_with_priorities(&request_info, &query, selected_priority_config, configured_priorities, flow_distinguisher)
        .await
    {
        Ok(permit) => permit,
        Err(crate::flowcontrol::limiter::Error::QueueFull) => {
            let response = json_response(
                StatusCode::TOO_MANY_REQUESTS,
                &too_many_requests_status(&path_str),
            );
            log_audit_rejected_request(
                &audit_id,
                &request_info,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                identity.as_ref(),
                &peer,
                response.status().as_u16(),
                audit_sink.as_deref(),
                audit_policy.as_deref(),
            );
            return Ok(response);
        }
        Err(crate::flowcontrol::limiter::Error::Closed) => {
            let response = json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &service_unavailable_status(
                    &path_str,
                    "API request concurrency limiter is unavailable",
                ),
            );
            log_audit_rejected_request(
                &audit_id,
                &request_info,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                identity.as_ref(),
                &peer,
                response.status().as_u16(),
                audit_sink.as_deref(),
                audit_policy.as_deref(),
            );
            return Ok(response);
        }
    };
    let _inflight = _permit
        .as_ref()
        .map(|_| metrics::begin_inflight(is_mutating_request(&request_info)));

    // Group M: `apiserver_request_duration_seconds`'s own start time —
    // measured around the exact same `handle()` call the audit event and
    // `apiserver_request_total` are both already keyed off of. For
    // `watch` specifically this measures time-to-first-byte (when
    // `handle()` returns the still-streaming response), not the full
    // stream lifetime.
    let start = std::time::Instant::now();
    let mut response_object = None;
    let mut response = match handle(req, storage, cache_registry, pure_admission, identity, service_account_authenticator, enforce_rbac, authorization_webhook_allowed, aggregation_proxy_identity, kubelet_tls).await {
        Ok(response) => {
            if audit_level == crate::audit::policy::Level::RequestResponse && !long_running {
                let (response, object) = capture_response_object(response, max_request_body_bytes).await;
                response_object = object;
                Ok(response)
            } else {
                Ok(response)
            }
        }
        Err(error) => match error {},
    };
    let elapsed = start.elapsed().as_secs_f64();

    if let Ok(resp) = &mut response {
        let metadata = admission_metadata.lock().map(|metadata| metadata.clone()).unwrap_or_default();
        apply_admission_warnings(resp, &metadata.warnings);
        let audit_annotations = audit_annotations(&metadata);
        let status = resp.status().as_u16();
        let request_object = request_body_capture
            .as_ref()
            .and_then(|capture| capture.lock().ok().and_then(|captured| captured.clone()))
            .as_deref()
            .and_then(|bytes| decode_audit_object(bytes, request_content_type.as_deref()));
        if audit_request_received && capture_request_body {
            log_audit_event_with_objects(
                &audit_id,
                crate::audit::event::STAGE_REQUEST_RECEIVED,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                audit_identity.as_ref(),
                &peer,
                0,
                audit_sink.as_deref(),
                &BTreeMap::new(),
                audit_level.as_str(),
                request_object.as_ref(),
                None,
            );
        }
        if audit_response_started {
            log_audit_event_with_objects(
                &audit_id,
                crate::audit::event::STAGE_RESPONSE_STARTED,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                audit_identity.as_ref(),
                &peer,
                status,
                audit_sink.as_deref(),
                &audit_annotations,
                audit_level.as_str(),
                request_object.as_ref(),
                None,
            );
        }
        if audit_response_complete {
            log_audit_event_with_objects(
                &audit_id,
                crate::audit::event::STAGE_RESPONSE_COMPLETE,
                &method,
                &path_str,
                &query,
                user_agent.as_deref(),
                audit_identity.as_ref(),
                &peer,
                status,
                audit_sink.as_deref(),
                &audit_annotations,
                audit_level.as_str(),
                request_object.as_ref(),
                response_object.as_ref(),
            );
        }
        // Group M: record the complete upstream-shaped metric label set from
        // the exact same parsed RequestInfo the audit event above builds.
        let info = &request_info;
        let metric_labels = metrics::labels_for_request(info, &query);
        metrics::record_request(&metric_labels, status);
        metrics::record_duration(&metric_labels, elapsed);
        // Group M: `apiserver_response_sizes` — only recorded when the
        // body's own size is known up front (`size_hint().exact()`,
        // `None` for a `watch`'s unbounded stream) — see `server::
        // metrics`'s own doc comment for why that's a real, named,
        // narrower scope than real upstream's own byte-counting
        // instrumentation, not a silent gap.
        {
            use http_body::Body as _;
            if let Some(size) = resp.body().size_hint().exact() {
                metrics::record_response_size(&metric_labels, size);
            }
        }

        // Group M (APF): label the response with the FlowSchema and
        // PriorityLevelConfiguration selected before the request entered
        // the bounded concurrency gate.
        if let Some(selected) = selected_priority {
            if let (Ok(fs), Ok(pl)) = (
                hyper::header::HeaderValue::from_str(&selected.flow_schema_uid),
                hyper::header::HeaderValue::from_str(&selected.priority_level_uid),
            ) {
                resp.headers_mut().insert(flowcontrol::resolve::FLOW_SCHEMA_UID_HEADER, fs);
                resp.headers_mut().insert(flowcontrol::resolve::PRIORITY_LEVEL_UID_HEADER, pl);
            }
        }
    }
    response
}

fn is_mutating_request(info: &path::RequestInfo) -> bool {
    matches!(
        info.verb.as_str(),
        "create" | "update" | "patch" | "delete" | "deletecollection"
    )
}

fn is_long_running_request(info: &path::RequestInfo, query: &str) -> bool {
    if matches!(info.verb.as_str(), "watch" | "proxy")
        || matches!(info.subresource.as_str(), "exec" | "attach" | "portforward")
    {
        return true;
    }
    info.subresource == "log"
        && path::parse_query(query).iter().any(|(key, value)| {
            key == "follow" && !matches!(value.as_str(), "" | "0" | "false")
        })
}

fn log_audit_event(
    audit_id: &str,
    stage: &str,
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    audit_sink: Option<&crate::audit::sink::AuditSink>,
    annotations: &BTreeMap<String, String>,
) {
    log_audit_event_with_objects(
        audit_id,
        stage,
        method,
        path_str,
        query,
        user_agent,
        identity,
        peer,
        status,
        audit_sink,
        annotations,
        crate::audit::event::LEVEL_METADATA,
        None,
        None,
    );
}

fn log_audit_event_with_objects(
    audit_id: &str,
    stage: &str,
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    audit_sink: Option<&crate::audit::sink::AuditSink>,
    annotations: &BTreeMap<String, String>,
    level: &str,
    request_object: Option<&Value>,
    response_object: Option<&Value>,
) {
    let event = build_audit_event_at_stage_with_objects(
        audit_id,
        stage,
        method,
        path_str,
        query,
        user_agent,
        identity,
        peer,
        status,
        annotations,
        level,
        request_object,
        response_object,
    );
    if let Some(sink) = audit_sink {
        if let Err(error) = sink.write(&event) {
            warn!(error = ?error, "nodeapiserver: failed to write audit event");
        }
    }
    tracing::info!(target: "nodeapiserver::audit", "{event}");
}

fn log_audit_rejected_request(
    audit_id: &str,
    info: &path::RequestInfo,
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    audit_sink: Option<&crate::audit::sink::AuditSink>,
    audit_policy: Option<&crate::audit::policy::AuditPolicy>,
) {
    let (user_name, user_groups): (&str, Vec<String>) = match identity {
        Some(identity) => (identity.name.as_str(), identity.groups.clone()),
        None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
    };
    if audit_policy.is_some_and(|policy| {
        policy.should_emit_stage(
            info,
            user_name,
            &user_groups,
            crate::audit::event::STAGE_REQUEST_RECEIVED,
        )
    }) {
        log_audit_event(
            audit_id,
            crate::audit::event::STAGE_REQUEST_RECEIVED,
            method,
            path_str,
            query,
            user_agent,
            identity,
            peer,
            0,
            audit_sink,
            &BTreeMap::new(),
        );
    }
    if audit_policy.map_or(true, |policy| {
        policy.should_emit_stage(
            info,
            user_name,
            &user_groups,
            crate::audit::event::STAGE_RESPONSE_COMPLETE,
        )
    }) {
        log_audit_event(
            audit_id,
            crate::audit::event::STAGE_RESPONSE_COMPLETE,
            method,
            path_str,
            query,
            user_agent,
            identity,
            peer,
            status,
            audit_sink,
            &BTreeMap::new(),
        );
    }
}

/// The pure half of [`log_audit_event`] — everything up to the built
/// `Value`, factored out so it's unit-testable without capturing
/// `tracing`'s own log output.
#[cfg(test)]
fn build_audit_event(
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    annotations: &BTreeMap<String, String>,
) -> serde_json::Value {
    let audit_id = uuid::Uuid::new_v4().to_string();
    build_audit_event_at_stage(
        &audit_id,
        crate::audit::event::STAGE_RESPONSE_COMPLETE,
        method,
        path_str,
        query,
        user_agent,
        identity,
        peer,
        status,
        annotations,
    )
}

#[cfg(test)]
fn build_audit_event_at_stage(
    audit_id: &str,
    stage: &str,
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    annotations: &BTreeMap<String, String>,
) -> serde_json::Value {
    build_audit_event_at_stage_with_objects(
        audit_id,
        stage,
        method,
        path_str,
        query,
        user_agent,
        identity,
        peer,
        status,
        annotations,
        crate::audit::event::LEVEL_METADATA,
        None,
        None,
    )
}

fn build_audit_event_at_stage_with_objects(
    audit_id: &str,
    stage: &str,
    method: &str,
    path_str: &str,
    query: &str,
    user_agent: Option<&str>,
    identity: Option<&crate::authn::x509::Identity>,
    peer: &SocketAddr,
    status: u16,
    annotations: &BTreeMap<String, String>,
    level: &str,
    request_object: Option<&Value>,
    response_object: Option<&Value>,
) -> serde_json::Value {
    let info = path::parse(method, path_str, query);
    let anonymous_extra = BTreeMap::new();
    let (user_name, user_uid, user_groups, user_extra): (&str, Option<&str>, Vec<String>, &BTreeMap<String, Vec<String>>) = match identity {
        Some(id) => (id.name.as_str(), id.uid.as_deref(), id.groups.clone(), &id.extra),
        None => (ANONYMOUS_USERNAME, None, vec![UNAUTHENTICATED_GROUP.to_string()], &anonymous_extra),
    };
    let object_ref = info.is_resource_request.then(|| crate::audit::event::ObjectRef { group: &info.api_group, resource: &info.resource, namespace: &info.namespace, name: &info.name, api_version: &info.api_version });
    let request_uri = if query.is_empty() { path_str.to_string() } else { format!("{path_str}?{query}") };
    let timestamp = chrono::Utc::now().to_rfc3339();
    let source_ip = peer.ip().to_string();
    crate::audit::event::build_event_at_stage_with_level(&crate::audit::event::EventInput {
        audit_id,
        request_uri: &request_uri,
        verb: &info.verb,
        user_name,
        user_uid,
        user_groups: user_groups.as_slice(),
        user_extra,
        source_ip: Some(&source_ip),
        user_agent,
        object_ref,
        response_code: status,
        annotations: (!annotations.is_empty()).then_some(annotations),
        request_object,
        response_object,
        timestamp: &timestamp,
    }, level, stage)
}

async fn authenticate_request(
    req: &Request<Incoming>,
    client_cert_identity: Option<crate::authn::x509::Identity>,
    bootstrap_token_authenticator: Option<&crate::authn::bootstrap_token::ReloadableAuthenticator>,
    service_account_authenticator: Option<&crate::authn::service_account::ReloadableAuthenticator>,
    oidc_authenticator: Option<&crate::authn::oidc::Authenticator>,
    anonymous_auth: bool,
) -> std::result::Result<Option<crate::authn::x509::Identity>, &'static str> {
    if client_cert_identity.is_some() {
        return Ok(client_cert_identity);
    }
    let Some(header) = req.headers().get("authorization") else {
        return if anonymous_auth { Ok(None) } else { Err("anonymous authentication is disabled") };
    };
    let value = header.to_str().map_err(|_| "Authorization header is not valid UTF-8")?;
    let Some(token) = value.strip_prefix("Bearer ").filter(|token| !token.is_empty()) else {
        return Err("Authorization must use the Bearer scheme");
    };
    if let Some(authenticator) = bootstrap_token_authenticator {
        if let Some(authenticated) = authenticator.authenticate(token) {
            return Ok(Some(authenticated.identity));
        }
    }
    if let Some(authenticator) = service_account_authenticator {
        if let Some(authenticated) = authenticator.authenticate(token) {
            return Ok(Some(authenticated.identity));
        }
    }
    if let Some(authenticator) = oidc_authenticator {
        if let Some(identity) = authenticator.authenticate(token).await {
            return Ok(Some(identity));
        }
    }
    if bootstrap_token_authenticator.is_none()
        && service_account_authenticator.is_none()
        && oidc_authenticator.is_none()
    {
        return Err("bearer-token authentication is not configured");
    }
    Err("bearer token is invalid or expired")
}

/// Return the namespace segment used by the REST storage key.
///
/// The upstream-compatible path parser keeps the second segment of
/// `/api/v1/namespaces/{name}` in `RequestInfo::namespace`, even though a
/// Namespace object is cluster-scoped. Do not turn that object name into a
/// storage namespace.
fn storage_namespace(info: &path::RequestInfo) -> Option<&str> {
    if info.namespace.is_empty()
        || (info.api_group.is_empty() && info.api_version == "v1" && info.resource == "namespaces")
    {
        None
    } else {
        Some(info.namespace.as_str())
    }
}

/// Run the immutable pure-mutator registry against the candidate produced by
/// any write-shaped REST path. Keeping this call at the candidate boundary
/// prevents ordinary CREATE/UPDATE, PATCH, and Server-Side Apply from
/// accidentally observing different admission behavior.
fn run_pure_admission(
    registry: &crate::admission::chain::MutatingRegistry,
    operation: admission::attributes::Operation,
    info: &path::RequestInfo,
    object: &mut Value,
) {
    let mut request = admission::chain::Request {
        operation,
        group: &info.api_group,
        resource: &info.resource,
        subresource: &info.subresource,
        namespace: &info.namespace,
        name: &info.name,
        object,
    };
    registry.run(&mut request);
}

async fn handle(
    req: Request<Incoming>,
    mut storage: Option<StorageClient>,
    cache_registry: crate::cacher::CacheRegistry,
    pure_admission: Arc<crate::admission::chain::MutatingRegistry>,
    identity: Option<crate::authn::x509::Identity>,
    service_account_authenticator: Option<Arc<crate::authn::service_account::ReloadableAuthenticator>>,
    enforce_rbac: bool,
    authorization_webhook_allowed: bool,
    aggregation_proxy_identity: Option<Arc<crate::aggregator::client_tls::ClientIdentity>>,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Result<Response<BoxedBody>, Infallible> {
    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let request_field_manager = field_manager_query(&query)
        .or_else(|| req.headers().get("user-agent").and_then(|value| value.to_str().ok()).map(str::to_string))
        .filter(|value| !value.is_empty());
    let admission_metadata = req.extensions().get::<SharedAdmissionMetadata>().cloned();

    let info = path::parse(&method, &path_str, &query);

    // Keep authorization ahead of every handler, including health, metrics,
    // and discovery endpoints. These are non-resource requests, but
    // upstream RBAC evaluates them through `nonResourceURLs` just like a
    // resource request is evaluated through `resources`.
    if should_run_local_authorization(&info, enforce_rbac, authorization_webhook_allowed) {
        let Some(client) = storage.as_mut() else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let allowed = match authz::request_allowed(client, identity.as_ref(), &info).await {
            Ok(allowed) => allowed,
            Err(error) => {
                warn!(path = %path_str, error = %error, "node/RBAC authorization failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        if !allowed {
            let user_name = identity.as_ref().map(|id| id.name.as_str()).unwrap_or(ANONYMOUS_USERNAME);
            return Ok(json_response(StatusCode::FORBIDDEN, &forbidden_status(&path_str, user_name)));
        }
    }

    if let Some(check_name) = path_str.strip_prefix('/').filter(|p| matches!(*p, "healthz" | "readyz" | "livez")) {
        let params = path::parse_query(&query);
        let verbose = params.iter().any(|(key, _)| key == "verbose");
        let excluded = params.into_iter().filter(|(key, _)| key == "exclude").map(|(_, value)| value).collect::<Vec<_>>();
        let storage_healthy = if check_name == "readyz" {
            match storage.as_mut() {
                Some(client) => tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    client.is_healthy(),
                )
                .await
                .unwrap_or(false),
                None => false,
            }
        } else {
            storage.is_some()
        };
        let (checks, unknown_excluded) = healthz::run_checks(check_name, storage_healthy, &excluded);
        let (status, body) = healthz::render(check_name, &checks, &unknown_excluded, verbose);
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
            DiscoveryRoute::Found(doc) => return Ok(json_response_with_content_type(StatusCode::OK, &doc, discovery_content_type(&parts, accept_header))),
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
                            return Ok(aggregate_proxy(req, &method, &api_service, client, &path_str, &query, identity.as_ref(), aggregation_proxy_identity.as_deref()).await);
                        }
                    }
                }
                return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
            }
            DiscoveryRoute::NotApplicable => {}
        }
    }

    // Resource requests cannot be classified as found or missing without a
    // nodestore connection. Report the unavailable backend instead of
    // allowing the generic not-found path to turn an outage into a false
    // successful server response.
    if info.is_resource_request && storage.is_none() {
        return Ok(json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &service_unavailable_status(&path_str, "storage backend unavailable"),
        ));
    }

    // Group N: the core node and Service proxy subresources are ordinary
    // request/response relays.  Keep them ahead of the generic REST
    // branches below: `GET .../services/name/proxy` otherwise looks like a
    // normal GET with an unknown subresource and would be returned as a
    // bring-up-shaped error instead of reaching the selected backend.
    if info.is_resource_request
        && info.api_group.is_empty()
        && matches!(info.resource.as_str(), "nodes" | "services")
        && is_proxy_request(&info)
        && !info.name.is_empty()
        && matches!(method.as_str(), "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS")
    {
        return Ok(proxy_resource(req, storage, &info, &method, &path_str, &query, &identity, enforce_rbac, kubelet_tls).await);
    }

    // Group E's real resource verbs so far: single-object GET (`get`, not
    // `list`/`watch` — `path::parse` already tells those apart by an empty
    // `name`), LIST (`list`, no name), CREATE (`create`, no name — a POST
    // to the collection URL), single-object DELETE (`delete`, name
    // required — no name means `deletecollection`, now real too — see its
    // own dedicated branch below), and UPDATE (`update`, name
    // required — a PUT). The scheduler's core `pods/binding` and Pod
    // `pods/ephemeralcontainers` subresources are handled separately below;
    // the remaining subresources still fall through (see `rest`'s own doc
    // comment). Everything else returns a real Kubernetes error below. `storage` is only
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

    // The scheduler binds a pending Pod through the real core
    // `pods/binding` subresource rather than replacing the whole Pod. This
    // must run before generic CRUD dispatch: `Binding` contains only the
    // selected Node and optional binding preconditions, while the REST
    // operation itself atomically updates the stored Pod.
    if info.is_resource_request
        && info.api_group.is_empty()
        && info.api_version == "v1"
        && info.resource == "pods"
        && info.subresource == "binding"
        && info.verb == "create"
        && !info.name.is_empty()
    {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        if info.namespace.is_empty() {
            return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "Pod binding requires a namespace")));
        }
        let body_bytes = match read_body_bytes(req).await {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(path = %path_str, error = ?error, "reading the Pod binding request failed");
                return Ok(body_read_error_response(&path_str, &error));
            }
        };
        let body: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(body) => body,
            Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
        };
        return match rest::bind_pod(&mut client, &info.namespace, &info.name, &body).await {
            Ok(rest::BindOutcome::Bound) => Ok(json_response(
                StatusCode::CREATED,
                &serde_json::json!({
                    "kind": "Status",
                    "apiVersion": "v1",
                    "metadata": {},
                    "status": "Success",
                    "code": 201,
                }),
            )),
            Ok(rest::BindOutcome::UnknownResource) | Ok(rest::BindOutcome::ObjectNotFound) => {
                Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)))
            }
            Ok(rest::BindOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &precondition_failed_status(&path_str))),
            Ok(rest::BindOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
            Err(error) => {
                warn!(path = %path_str, error = ?error, "rest::bind_pod failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }

    // The core Pod `ephemeralcontainers` subresource has its own update
    // strategy: GET returns the Pod, while PUT/PATCH may change only
    // `spec.ephemeralContainers`. The REST helpers reset every other field
    // and reject removal or mutation of an existing ephemeral container
    // before using the normal MVCC write path.
    if info.is_resource_request
        && info.api_group.is_empty()
        && info.api_version == "v1"
        && info.resource == "pods"
        && info.subresource == "ephemeralcontainers"
        && !info.name.is_empty()
    {
        if info.namespace.is_empty() {
            return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "Pod ephemeralcontainers requires a namespace")));
        }
        if info.verb == "get" {
            let Some(mut client) = storage else {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            };
            return match rest::get_ephemeral_containers(&mut client, &info.namespace, &info.name).await {
                Ok(rest::GetOutcome::Found(object)) => Ok(json_response(StatusCode::OK, &object)),
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "rest::get_ephemeral_containers failed");
                    Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
                }
            };
        }

        if info.verb == "update" || info.verb == "patch" {
            let Some(mut client) = storage else {
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            };
            let existing_pod = match rest::get(&mut client, None, "", "v1", "pods", Some(&info.namespace), &info.name).await {
                Ok(rest::GetOutcome::Found(pod)) => pod,
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                    return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                }
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "admission: reading the Pod for ephemeralcontainers failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            let Some(service_account_name) = existing_pod
                .pointer("/spec/serviceAccountName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            else {
                return Ok(json_response(
                    StatusCode::FORBIDDEN,
                    &admission_forbidden_status(
                        &path_str,
                        &format!(
                            "no service account specified for pod {}/{}",
                            info.namespace, info.name
                        ),
                    ),
                ));
            };
            let service_account = match rest::get(
                &mut client,
                None,
                "",
                "v1",
                "serviceaccounts",
                Some(&info.namespace),
                service_account_name,
            )
            .await
            {
                Ok(rest::GetOutcome::Found(service_account)) => service_account,
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                    return Ok(json_response(
                        StatusCode::FORBIDDEN,
                        &admission_forbidden_status(
                            &path_str,
                            &format!(
                                "error looking up service account {}/{}: not found",
                                info.namespace, service_account_name
                            ),
                        ),
                    ));
                }
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "admission: reading the ServiceAccount for ephemeralcontainers failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            let dry_run = match dry_run_query(&query) {
                Ok(value) => value,
                Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
            };
            let content_type = req.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_string);
            let body_bytes = match read_body_bytes(req).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "reading the ephemeralcontainers request failed");
                    return Ok(body_read_error_response(&path_str, &error));
                }
            };
            let body: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
                Ok(body) => body,
                Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
            };
            let validate_ephemeral = |pod: &Value| {
                admission::service_account::validate_ephemeral_container_secret_references(
                    &service_account,
                    pod,
                )
                .map_err(|error| vec![error])
            };
            let outcome = if info.verb == "update" {
                rest::update_ephemeral_containers(&mut client, &info.namespace, &info.name, &body, dry_run, request_field_manager.as_deref(), validate_ephemeral).await
            } else {
                let kind_of_patch = match content_type.as_deref() {
                    Some(content_type) => match rest::patch_kind_for_content_type(content_type) {
                        Some(kind) => kind,
                        None => {
                            return Ok(json_response(
                                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                                &bad_request_status(&path_str, "unsupported Content-Type for the ephemeralcontainers subresource"),
                            ));
                        }
                    },
                    None => rest::PatchKind::StrategicMerge,
                };
                rest::patch_ephemeral_containers(&mut client, &info.namespace, &info.name, kind_of_patch, &body, dry_run, request_field_manager.as_deref(), validate_ephemeral).await
            };
            return match outcome {
                Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
                Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
                Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                Ok(rest::UpdateOutcome::MissingResourceVersion) | Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str))),
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "ephemeralcontainers update failed");
                    Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
                }
            };
        }
    }

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
            let dry_run = match dry_run_query(&query) {
                Ok(value) => value,
                Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
            };
            let body_bytes = match read_body_bytes(req).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "reading the request body failed");
                    return Ok(body_read_error_response(&path_str, &e));
                }
            };
            let config: serde_json::Value = match crate::codec::yaml::decode(&body_bytes) {
                Ok(v) => v,
                Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
            };
            let namespace = storage_namespace(&info);

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

            let (mut candidate, apply_context) = match rest::apply_prepare(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, &manager, force, &config).await {
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

            let old_object = match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                Ok(rest::GetOutcome::Found(object)) => Some(object),
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "admission: reading the existing object for apply webhooks failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            let operation = if old_object.is_some() {
                admission::attributes::Operation::Update
            } else {
                admission::attributes::Operation::Create
            };

            // Node authorization cannot inspect an apply candidate's body.
            // Run the same body-sensitive NodeRestriction check as ordinary
            // writes before any mutating admission changes the candidate.
            if authz::node::node_name(identity.as_ref()).is_some() {
                match admission::node_restriction::validate(
                    &mut client,
                    identity.as_ref(),
                    operation,
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                    &info.namespace,
                    &info.name,
                    Some(&candidate),
                    old_object.as_ref(),
                )
                .await
                {
                    Ok(()) => {}
                    Err(admission::node_restriction::Error::Forbidden(message)) => {
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&path_str, &message),
                        ));
                    }
                    Err(admission::node_restriction::Error::Lookup(error)) => {
                        warn!(path = %path_str, error = %error, "admission: NodeRestriction lookup failed for apply");
                        return Ok(json_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &internal_error_status(&path_str),
                        ));
                    }
                }
            }
            run_pure_admission(&pure_admission, operation, &info, &mut candidate);

            // Apply must run the storage-backed DefaultStorageClass plugin
            // against the materialized candidate too. A PVC with no class
            // is otherwise persisted differently depending on whether its
            // creator used POST or Server-Side Apply.
            if operation == admission::attributes::Operation::Create
                && admission::default_storage_class::applies_to(
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                )
            {
                match rest::list(
                    &mut client,
                    None,
                    "storage.k8s.io",
                    "v1",
                    "storageclasses",
                    None,
                    "",
                    "",
                    0,
                    "",
                )
                .await
                {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let classes = list["items"].as_array().cloned().unwrap_or_default();
                        admission::default_storage_class::mutate(&mut candidate, &classes);
                    }
                    Ok(rest::ListOutcome::UnknownResource)
                    | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                    Err(error) => {
                        warn!(path = %path_str, error = ?error, "admission: listing StorageClasses for apply failed");
                        return Ok(json_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &internal_error_status(&path_str),
                        ));
                    }
                }
            }

            // Apply must also run DefaultIngressClass against the candidate.
            // Otherwise an Ingress created with POST and one created with
            // Server-Side Apply receive different class defaulting.
            if operation == admission::attributes::Operation::Create
                && admission::default_ingress_class::applies_to(
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                )
            {
                match rest::list(
                    &mut client,
                    None,
                    "networking.k8s.io",
                    "v1",
                    "ingressclasses",
                    None,
                    "",
                    "",
                    0,
                    "",
                )
                .await
                {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let classes = list["items"].as_array().cloned().unwrap_or_default();
                        admission::default_ingress_class::mutate(&mut candidate, &classes);
                    }
                    Ok(rest::ListOutcome::UnknownResource)
                    | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                    Err(error) => {
                        warn!(path = %path_str, error = ?error, "admission: listing IngressClasses for apply failed");
                        return Ok(json_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &internal_error_status(&path_str),
                        ));
                    }
                }
            }

            // StorageObjectInUseProtection is a pure create-time mutator,
            // but Apply still has to invoke it so PV/PVC/VAC objects do not
            // lose their protection finalizer merely because they were
            // submitted with Server-Side Apply.
            if operation == admission::attributes::Operation::Create {
                admission::storage_object_in_use_protection::mutate(
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                    &mut candidate,
                );
            }

            // RuntimeClass mutates and validates ordinary Pod creates. Apply
            // must resolve the same class before policy/webhook admission so
            // its overhead and scheduling fields are part of the candidate.
            if operation == admission::attributes::Operation::Create
                && admission::runtime_class::applies_to(
                    admission::attributes::Operation::Create,
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                )
            {
                let runtime_class_name = candidate
                    .pointer("/spec/runtimeClassName")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let runtime_class = if let Some(runtime_class_name) = runtime_class_name {
                    match rest::get(
                        &mut client,
                        None,
                        "node.k8s.io",
                        "v1",
                        "runtimeclasses",
                        None,
                        &runtime_class_name,
                    )
                    .await
                    {
                        Ok(rest::GetOutcome::Found(runtime_class)) => Some(runtime_class),
                        Ok(rest::GetOutcome::ObjectNotFound)
                        | Ok(rest::GetOutcome::UnknownResource) => {
                            return Ok(json_response(
                                StatusCode::FORBIDDEN,
                                &admission_forbidden_status(
                                    &path_str,
                                    &format!(
                                        "pod rejected: RuntimeClass {runtime_class_name:?} not found"
                                    ),
                                ),
                            ));
                        }
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission: RuntimeClass lookup for apply failed");
                            return Ok(json_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &internal_error_status(&path_str),
                            ));
                        }
                    }
                } else {
                    None
                };
                if let Err(error) =
                    admission::runtime_class::mutate_and_validate(&mut candidate, runtime_class.as_ref())
                {
                    return Ok(json_response(
                        StatusCode::FORBIDDEN,
                        &admission_forbidden_status(&path_str, &error),
                    ));
                }
            }

            // The pure registry only supplies the default ServiceAccount
            // name. Complete the storage-backed ServiceAccount plugin for
            // create-on-apply as well, so applied Pods receive the same
            // token-volume, automount, imagePullSecret, and secret-reference
            // handling as ordinary Pod CREATE.
            if operation == admission::attributes::Operation::Create
                && admission::service_account::applies_to(
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                )
            {
                match admission::service_account::quick_decision(
                    &candidate,
                    admission::attributes::Operation::Create,
                ) {
                    admission::service_account::Decision::Allow => {}
                    admission::service_account::Decision::Forbidden(message) => {
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&path_str, &message),
                        ));
                    }
                    admission::service_account::Decision::NeedsServiceAccountLookup => {
                        let service_account_name = candidate
                            .pointer("/spec/serviceAccountName")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        match rest::get(
                            &mut client,
                            None,
                            "",
                            "v1",
                            "serviceaccounts",
                            namespace,
                            &service_account_name,
                        )
                        .await
                        {
                            Ok(rest::GetOutcome::Found(service_account)) => {
                                admission::service_account::mutate_with_service_account(
                                    &mut candidate,
                                    &service_account,
                                    || {
                                        let suffix: String = uuid::Uuid::new_v4()
                                            .to_string()
                                            .chars()
                                            .take(5)
                                            .collect();
                                        format!(
                                            "{}{suffix}",
                                            admission::service_account::SERVICE_ACCOUNT_VOLUME_PREFIX
                                        )
                                    },
                                );
                                if let Err(error) =
                                    admission::service_account::validate_secret_references(
                                        &service_account,
                                        &candidate,
                                    )
                                {
                                    return Ok(json_response(
                                        StatusCode::FORBIDDEN,
                                        &admission_forbidden_status(&path_str, &error),
                                    ));
                                }
                            }
                            Ok(rest::GetOutcome::ObjectNotFound)
                            | Ok(rest::GetOutcome::UnknownResource) => {
                                return Ok(json_response(
                                    StatusCode::FORBIDDEN,
                                    &admission_forbidden_status(
                                        &path_str,
                                        &format!(
                                            "error looking up service account {:?}/{:?}: not found",
                                            info.namespace, service_account_name
                                        ),
                                    ),
                                ));
                            }
                            Err(error) => {
                                warn!(path = %path_str, error = ?error, "admission: service account lookup for apply failed");
                                return Ok(json_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    &internal_error_status(&path_str),
                                ));
                            }
                        }
                    }
                }
            }

            // MutatingAdmissionPolicy is part of the same candidate-based
            // admission chain as ordinary CREATE/UPDATE. Apply must not
            // bypass it merely because its field-management preparation
            // happens in a separate REST helper.
            let operation_name = match operation {
                admission::attributes::Operation::Create => "CREATE",
                admission::attributes::Operation::Update => "UPDATE",
                _ => unreachable!("Server-Side Apply is create- or update-shaped"),
            };
            match admission::mutating_admission_policy::mutate(
                &mut client,
                operation_name,
                &info.api_group,
                &info.api_version,
                &info.resource,
                &info.subresource,
                &info.namespace,
                &info.name,
                candidate,
                old_object.as_ref(),
                dry_run,
                identity.as_ref(),
            )
            .await
            {
                Ok(admitted) => candidate = admitted,
                Err(error) => {
                    warn!(path = %path_str, error, "admission: MutatingAdmissionPolicy failed for apply");
                    return Ok(json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &internal_error_status(&path_str),
                    ));
                }
            }
            match admission::policy_enforcement::validate(
                &mut client,
                operation_name,
                &info.api_group,
                &info.api_version,
                &info.resource,
                &info.subresource,
                &info.namespace,
                &info.name,
                Some(&candidate),
                old_object.as_ref(),
                dry_run,
                identity.as_ref(),
            )
            .await
            {
                Ok(outcome) => {
                    record_admission_outcome(admission_metadata.as_ref(), &outcome);
                    if let Some(message) = outcome.denial {
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&path_str, &message),
                        ));
                    }
                }
                Err(error) => {
                    warn!(path = %path_str, error, "admission: ValidatingAdmissionPolicy failed for apply");
                    return Ok(json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &internal_error_status(&path_str),
                    ));
                }
            }
            match admission::webhook::admit(
                &mut client,
                operation,
                &info.api_group,
                &info.api_version,
                &info.resource,
                &info.subresource,
                &info.namespace,
                &info.name,
                candidate.clone(),
                old_object,
                identity.as_ref(),
                dry_run,
            )
            .await
            {
                Ok(admission::webhook::Outcome::Allowed(admitted)) => candidate = admitted,
                Ok(admission::webhook::Outcome::Denied(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                }
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "admission webhook invocation failed for apply");
                    return Ok(admission_webhook_error_response(&path_str, &error));
                }
            }

            return match rest::apply_persist(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, apply_context, candidate, dry_run).await {
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

        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let dry_run = match dry_run_query(&query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
        };
        let kind_of_patch = match content_type.as_deref() {
            Some(content_type) => match rest::patch_kind_for_content_type(content_type) {
                Some(kind) => kind,
                None => {
                    return Ok(json_response(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        &bad_request_status(&path_str, "unsupported Content-Type for PATCH -- use application/json-patch+json, application/merge-patch+json, or application/strategic-merge-patch+json"),
                    ));
                }
            },
            None => match rest::default_patch_kind_for_request(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                Ok(Some(kind)) => kind,
                Ok(None) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "resolving the default PATCH strategy failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            },
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(body_read_error_response(&path_str, &e));
            }
        };
        let patch_doc: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let namespace = storage_namespace(&info);

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

        let (mut candidate, context) = match rest::patch_prepare(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, kind_of_patch, &patch_doc).await {
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

        // `PersistentVolumeClaimResize` also runs after a PATCH has been
        // materialized into an Update candidate. Re-read the old object so
        // the same bound-claim and StorageClass checks cover both PUT and
        // PATCH request shapes.
        if admission::pvc_resize::applies_to(
            admission::attributes::Operation::Update,
            &info.api_group,
            &info.resource,
            &info.subresource,
        ) {
            let old_pvc = match rest::get(&mut client, None, "", "v1", "persistentvolumeclaims", namespace, &info.name).await {
                Ok(rest::GetOutcome::Found(old_pvc)) => old_pvc,
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => Value::Null,
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "admission: reading the existing PVC for patch resize failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            match rest::list(&mut client, None, "storage.k8s.io", "v1", "storageclasses", None, "", "", 0, "").await {
                Ok(rest::ListOutcome::Found(list)) => {
                    let classes = list["items"].as_array().cloned().unwrap_or_default();
                    if let Err(error) = admission::pvc_resize::validate_resize(&candidate, &old_pvc, &classes) {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                    }
                }
                Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {
                    if let Err(error) = admission::pvc_resize::validate_resize(&candidate, &old_pvc, &[]) {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                    }
                }
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "admission: listing StorageClasses for patch resize failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }
        }

        let old_object = match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
            Ok(rest::GetOutcome::Found(object)) => Some(object),
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "admission: reading the existing object for patch webhooks failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        run_pure_admission(
            &pure_admission,
            admission::attributes::Operation::Update,
            &info,
            &mut candidate,
        );
        match admission::mutating_admission_policy::mutate(
            &mut client,
            "UPDATE",
            &info.api_group,
            &info.api_version,
            &info.resource,
            &info.subresource,
            &info.namespace,
            &info.name,
            candidate,
            old_object.as_ref(),
            dry_run,
            identity.as_ref(),
        )
        .await
        {
            Ok(admitted) => candidate = admitted,
            Err(error) => {
                warn!(path = %path_str, error, "admission: MutatingAdmissionPolicy failed for patch");
                return Ok(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &internal_error_status(&path_str),
                ));
            }
        }
        match admission::policy_enforcement::validate(
            &mut client,
            "UPDATE",
            &info.api_group,
            &info.api_version,
            &info.resource,
            &info.subresource,
            &info.namespace,
            &info.name,
            Some(&candidate),
            old_object.as_ref(),
            dry_run,
            identity.as_ref(),
        )
        .await
        {
            Ok(outcome) => {
                record_admission_outcome(admission_metadata.as_ref(), &outcome);
                if let Some(message) = outcome.denial {
                    return Ok(json_response(
                        StatusCode::FORBIDDEN,
                        &admission_forbidden_status(&path_str, &message),
                    ));
                }
            }
            Err(error) => {
                warn!(path = %path_str, error, "admission: ValidatingAdmissionPolicy failed for patch");
                return Ok(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &internal_error_status(&path_str),
                ));
            }
        }
        if authz::node::node_name(identity.as_ref()).is_some() {
            match admission::node_restriction::validate(
                &mut client,
                identity.as_ref(),
                admission::attributes::Operation::Update,
                &info.api_group,
                &info.resource,
                &info.subresource,
                &info.namespace,
                &info.name,
                Some(&candidate),
                old_object.as_ref(),
            )
            .await
            {
                Ok(()) => {}
                Err(admission::node_restriction::Error::Forbidden(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                }
                Err(admission::node_restriction::Error::Lookup(error)) => {
                    warn!(path = %path_str, error = %error, "admission: NodeRestriction lookup failed for patch");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }
        }
        match admission::webhook::admit(
            &mut client,
            admission::attributes::Operation::Update,
            &info.api_group,
            &info.api_version,
            &info.resource,
            &info.subresource,
            &info.namespace,
            &info.name,
            candidate.clone(),
            old_object,
            identity.as_ref(),
            dry_run,
        )
        .await
        {
            Ok(admission::webhook::Outcome::Allowed(admitted)) => candidate = admitted,
            Ok(admission::webhook::Outcome::Denied(message)) => {
                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "admission webhook invocation failed for patch");
                return Ok(admission_webhook_error_response(&path_str, &error));
            }
        }

        return match rest::patch_persist_with_manager(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, context, candidate, dry_run, request_field_manager.as_deref()).await {
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
        let dry_run = match dry_run_query(&query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(body_read_error_response(&path_str, &e));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let namespace = storage_namespace(&info);
        if authz::node::node_name(identity.as_ref()).is_some() {
            let old_object = match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                Ok(rest::GetOutcome::Found(object)) => Some(object),
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "admission: reading the existing object for NodeRestriction status update failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            };
            match admission::node_restriction::validate(
                &mut client,
                identity.as_ref(),
                admission::attributes::Operation::Update,
                &info.api_group,
                &info.resource,
                &info.subresource,
                &info.namespace,
                &info.name,
                Some(&body_value),
                old_object.as_ref(),
            )
            .await
            {
                Ok(()) => {}
                Err(admission::node_restriction::Error::Forbidden(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                }
                Err(admission::node_restriction::Error::Lookup(error)) => {
                    warn!(path = %path_str, error = %error, "admission: NodeRestriction lookup failed for status update");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }
        }
        return match rest::update_status_with_manager(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, &body_value, dry_run, request_field_manager.as_deref()).await {
            Ok(rest::UpdateOutcome::Updated(object)) => Ok(json_response(StatusCode::OK, &object)),
            Ok(rest::UpdateOutcome::UnknownResource) | Ok(rest::UpdateOutcome::ObjectNotFound) => Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
            Ok(rest::UpdateOutcome::MissingResourceVersion) => {
                Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.resourceVersion is required for an update")))
            }
            Ok(rest::UpdateOutcome::Conflict) => Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
            Ok(rest::UpdateOutcome::Invalid(violations)) => Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
            // `rest::update_status` never itself returns these two -- it
            // does not check a body namespace, and `UnsupportedPatchType`
            // is `rest::patch`-only. Keep the match exhaustive rather than
            // turning a future implementation change into a panic.
            Ok(rest::UpdateOutcome::NamespaceMismatch) | Ok(rest::UpdateOutcome::UnsupportedPatchType) => {
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
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let dry_run = match dry_run_query(&query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
        };
        let kind_of_patch = match content_type.as_deref() {
            Some(content_type) => match rest::patch_kind_for_content_type(content_type) {
                Some(kind) => kind,
                None => {
                    return Ok(json_response(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        &bad_request_status(&path_str, "unsupported Content-Type for PATCH -- use application/json-patch+json, application/merge-patch+json, or application/strategic-merge-patch+json"),
                    ));
                }
            },
            None => match rest::default_patch_kind_for_request(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                Ok(Some(kind)) => kind,
                Ok(None) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "resolving the default PATCH strategy failed");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            },
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the request body failed");
                return Ok(body_read_error_response(&path_str, &e));
            }
        };
        let patch_doc: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let namespace = storage_namespace(&info);
        return match rest::patch_status_with_manager(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, kind_of_patch, &patch_doc, dry_run, request_field_manager.as_deref()).await {
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
    // `create`/`update`), and has to validate each selected object before
    // deleting it. This mirrors upstream's DeleteCollection handler, which
    // passes its delete validator into the store and lets the store invoke
    // it for every matched object.
    if info.is_resource_request && info.verb == "deletecollection" && info.subresource.is_empty() {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let namespace = if info.namespace.is_empty() { None } else { Some(info.namespace.as_str()) };
        let listed = match rest::list_delete_collection(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.label_selector, &info.field_selector).await {
            Ok(outcome) => outcome,
            Err(rest::Error::Selector(error)) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
            Err(error) => {
                warn!(path = %path_str, error = ?error, "rest::list_delete_collection failed");
                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
            }
        };
        let rest::DeleteCollectionOutcome::Deleted(list) = listed else {
            return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
        };
        let items = list.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        for item in &items {
            let Some(name) = item.pointer("/metadata/name").and_then(Value::as_str) else {
                continue;
            };

            // DeleteCollection's admission attributes intentionally retain
            // an empty request name, as upstream does; the selected object
            // is still supplied as oldObject to policy/webhook admission.
            match admission::policy_enforcement::validate(
                &mut client,
                "DELETE",
                &info.api_group,
                &info.api_version,
                &info.resource,
                &info.subresource,
                &info.namespace,
                "",
                None,
                Some(item),
                false,
                identity.as_ref(),
            )
            .await
            {
                Ok(outcome) => {
                    record_admission_outcome(admission_metadata.as_ref(), &outcome);
                    if let Some(message) = outcome.denial {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                    }
                }
                Err(error) => {
                    warn!(path = %path_str, error = %error, "admission: ValidatingAdmissionPolicy evaluation failed for deletecollection");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }

            match admission::webhook::admit(
                &mut client,
                admission::attributes::Operation::Delete,
                &info.api_group,
                &info.api_version,
                &info.resource,
                &info.subresource,
                &info.namespace,
                "",
                item.clone(),
                Some(item.clone()),
                identity.as_ref(),
                false,
            )
            .await
            {
                Ok(admission::webhook::Outcome::Allowed(_)) => {}
                Ok(admission::webhook::Outcome::Denied(message)) => {
                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                }
                Err(error) => {
                    warn!(path = %path_str, error = ?error, name, "admission webhook invocation failed for deletecollection");
                    return Ok(admission_webhook_error_response(&path_str, &error));
                }
            }

            match rest::delete(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, name).await {
                Ok(rest::DeleteOutcome::Deleted(_)) | Ok(rest::DeleteOutcome::ObjectNotFound) => {}
                Ok(rest::DeleteOutcome::UnknownResource) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                Ok(rest::DeleteOutcome::PreconditionFailed) => return Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
                Err(error) => {
                    warn!(path = %path_str, error = ?error, name, "rest::delete failed for deletecollection");
                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                }
            }
        }
        return Ok(json_response(StatusCode::OK, &list));
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
                return Ok(body_read_error_response(&path_str, &e));
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
                return Ok(body_read_error_response(&path_str, &e));
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
                return Ok(body_read_error_response(&path_str, &e));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let anonymous_extra = BTreeMap::new();
        let (username, uid, groups, extra): (&str, Option<&str>, Vec<String>, &BTreeMap<String, Vec<String>>) = match &identity {
            Some(id) => (id.name.as_str(), id.uid.as_deref(), id.groups.clone(), &id.extra),
            None => (ANONYMOUS_USERNAME, None, vec![UNAUTHENTICATED_GROUP.to_string()], &anonymous_extra),
        };
        let mut response_body = body_value;
        response_body["status"] = crate::authn::self_review::build_status(username, uid, &groups, extra);
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
        if storage.is_none() {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        }
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the TokenReview body failed");
                return Ok(body_read_error_response(&path_str, &e));
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
                    "extra": authenticated.identity.extra,
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
        let Some(authenticator) = service_account_authenticator.as_deref() else {
            return Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(&path_str, "ServiceAccount token signing is not configured")));
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path_str, error = ?e, "reading the TokenRequest body failed");
                return Ok(body_read_error_response(&path_str, &e));
            }
        };
        let body_value: serde_json::Value = match crate::codec::json::decode(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string()))),
        };
        let mut request = match crate::authn::service_account::parse_token_request(&body_value) {
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
                Ok(rest::GetOutcome::Found(pod)) if pod.pointer("/metadata/uid").and_then(serde_json::Value::as_str) == Some(pod_uid) => {
                    if let Some(node_name) = pod
                        .pointer("/spec/nodeName")
                        .and_then(serde_json::Value::as_str)
                        .filter(|name| !name.is_empty())
                    {
                        let node_uid = match rest::get(&mut client, None, "", "v1", "nodes", None, node_name).await {
                            Ok(rest::GetOutcome::Found(node)) => node
                                .pointer("/metadata/uid")
                                .and_then(serde_json::Value::as_str)
                                .filter(|uid| !uid.is_empty())
                                .map(str::to_string),
                            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                            Err(error) => {
                                warn!(error = ?error, node = %node_name, "TokenRequest node lookup failed; issuing the node-name claim without a node UID");
                                None
                            }
                        };
                        request.bound_pod_node = Some((node_name.to_string(), node_uid));
                    }
                }
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
    // Built-in workload scale subresources expose a virtual
    // `autoscaling/v1 Scale`, not the parent object itself. Keep this ahead
    // of generic CRUD so HPA and `kubectl scale` can read and update
    // `spec.replicas` without persisting a second object.
    if info.is_resource_request
        && info.subresource == "scale"
        && !info.name.is_empty()
        && rest::supports_scale(&info.api_group, &info.api_version, &info.resource)
        && matches!(info.verb.as_str(), "get" | "update" | "patch")
    {
        let Some(mut client) = storage else {
            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
        };
        let namespace = (!info.namespace.is_empty()).then_some(info.namespace.as_str());
        if info.verb == "get" {
            return match rest::get_scale(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                Ok(outcome) => Ok(scale_outcome_response(&path_str, outcome)),
                Err(error) => {
                    warn!(path = %path_str, error = ?error, "rest::get_scale failed");
                    Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
                }
            };
        }

        let content_type = req.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_string);
        let kind_of_patch = if info.verb == "patch" {
            match content_type.as_deref() {
                Some(content_type) => match rest::patch_kind_for_content_type(content_type) {
                    Some(kind) => Some(kind),
                    None => {
                        return Ok(json_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, &bad_request_status(&path_str, "unsupported Content-Type for the Scale subresource")));
                    }
                },
                None => Some(rest::PatchKind::StrategicMerge),
            }
        } else {
            None
        };
        let body_bytes = match read_body_bytes(req).await {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(path = %path_str, error = ?error, "reading the Scale request failed");
                return Ok(body_read_error_response(&path_str, &error));
            }
        };
        let body: Value = if info.verb == "update" && content_type.as_deref().and_then(negotiation::content_type) == Some(negotiation::Format::Yaml) {
            match crate::codec::yaml::decode(&body_bytes) {
                Ok(body) => body,
                Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
            }
        } else {
            match crate::codec::json::decode(&body_bytes) {
                Ok(body) => body,
                Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
            }
        };
        let dry_run = match dry_run_query(&query) {
            Ok(value) => value,
            Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
        };
        let outcome = if info.verb == "update" {
            rest::update_scale(
                &mut client,
                &info.api_group,
                &info.api_version,
                &info.resource,
                namespace,
                &info.name,
                &body,
                dry_run,
            )
            .await
        } else if let Some(kind_of_patch) = kind_of_patch {
            rest::patch_scale(
                &mut client,
                &info.api_group,
                &info.api_version,
                &info.resource,
                namespace,
                &info.name,
                kind_of_patch,
                &body,
                dry_run,
            )
            .await
        } else {
            unreachable!("scale PATCH requests always have a patch kind")
        };
        return match outcome {
            Ok(outcome) => Ok(scale_outcome_response(&path_str, outcome)),
            Err(error) => {
                warn!(path = %path_str, error = ?error, "rest::Scale update failed");
                Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)))
            }
        };
    }

    // Group L: aggregated APIs (`APIService`) — a genuine live reverse
    // proxy to a real aggregated backend, with discovery merge already
    // wired through the request-time discovery path.
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
    // `pods/log`'s own branch already established. Discovery-shaped
    // requests are handled above, including live `/apis/{group}/{version}`
    // enumeration; only resource-shaped requests under an already-known
    // `(group, version)` reach this branch, matching real upstream's own
    // "resource requests only" scope for its aggregation proxy handler.
    if info.is_resource_request && !info.api_group.is_empty() {
        if let Some(mut client) = storage.clone() {
            match aggregator::route::resolve(&mut client, &info.api_group, &info.api_version).await {
                Ok(Some(api_service)) => return Ok(aggregate_proxy(req, &method, &api_service, client, &path_str, &query, identity.as_ref(), aggregation_proxy_identity.as_deref()).await),
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

    // WATCH is dispatched below the CRUD block, so retain this negotiated
    // representation before the request body can be consumed by a mutating
    // request.
    let wants_partial_metadata = req
        .headers()
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .and_then(negotiation::negotiate)
        .is_some_and(|accepted| accepted.wants_partial_object_metadata());
    let has_body = is_create || is_update;
    if is_get || is_list || is_create || is_delete || is_update {
        // Captured before `req` is potentially consumed below (`has_body`
        // moves it into `read_body_bytes`) — a borrow of `req.headers()`
        // can't outlive that move.
        let content_type = req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        // Same reasoning — `GET`/`LIST`'s own `Table` negotiation
        // (`kubectl get`'s real default `Accept` header) needs this
        // after `req` may already be gone.
        let accepted = req.headers().get("accept").and_then(|v| v.to_str().ok()).and_then(negotiation::negotiate);
        let wants_table = accepted.as_ref().is_some_and(|a| a.wants_table());

        if let Some(mut client) = storage {
            let namespace = storage_namespace(&info);
            // ResourceQuota admission derives usage from a live object list
            // and persists the object later in this same request. Hold the
            // process-local reservation lock across that whole sequence so
            // concurrent namespaced creates cannot both pass against the
            // same pre-create snapshot.
            let _quota_admission_guard = if is_create && namespace.is_some() {
                Some(RESOURCE_QUOTA_ADMISSION_LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await)
            } else {
                None
            };
            let crd_printer_columns = if wants_table {
                match rest::resolve_dynamic_resource(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                    Ok(Some(resolved)) => Some(resolved.additional_printer_columns),
                    Ok(None) => None,
                    Err(error) => {
                        warn!(path = %path_str, error = ?error, "table response: failed to resolve CRD printer columns");
                        None
                    }
                }
            } else {
                None
            };

            let dry_run = if is_create || is_update || is_delete {
                match dry_run_query(&query) {
                    Ok(value) => value,
                    Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
                }
            } else {
                false
            };

            // CREATE/UPDATE carry a full submitted object; DELETE carries
            // DeleteOptions. Read the request exactly once because hyper's
            // incoming body is single-consumer.
            let (mut body_value, delete_options) = if has_body || is_delete {
                let body_bytes = match read_body_bytes(req).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "reading the request body failed");
                        return Ok(body_read_error_response(&path_str, &e));
                    }
                };
                if is_delete {
                    if body_bytes.is_empty() {
                        (None, None)
                    } else {
                        let format = content_type.as_deref().and_then(negotiation::content_type).unwrap_or(negotiation::Format::Json);
                        let decoded = match format {
                            negotiation::Format::Json => crate::codec::json::decode(&body_bytes).map_err(|e| e.to_string()),
                            negotiation::Format::Yaml => crate::codec::yaml::decode(&body_bytes).map_err(|e| e.to_string()),
                            negotiation::Format::Protobuf => Err("protobuf DELETE options are not decoded yet".to_string()),
                        };
                        match decoded {
                            Ok(value) => (None, Some(value)),
                            Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error))),
                        }
                    }
                } else {
                    let format = content_type.as_deref().and_then(negotiation::content_type).unwrap_or(negotiation::Format::Json);
                    let decoded: Result<serde_json::Value, String> = match format {
                        negotiation::Format::Json => crate::codec::json::decode(&body_bytes).map_err(|e| e.to_string()),
                        negotiation::Format::Yaml => crate::codec::yaml::decode(&body_bytes).map_err(|e| e.to_string()),
                        negotiation::Format::Protobuf => match rest::decode_protobuf_request(&mut client, &info.api_group, &info.api_version, &info.resource, &body_bytes).await {
                            Ok(Some(value)) => Ok(value),
                            Ok(None) => return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str))),
                            Err(error) => Err(error.to_string()),
                        },
                    };
                    match decoded {
                        Ok(value) => (Some(value), None),
                        Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error))),
                    }
                }
            } else {
                (None, None)
            };

            // Group I: the Node authorizer cannot inspect request bodies, so
            // NodeRestriction supplies the body-sensitive half of the same
            // upstream authorization chain. Fetch the old object only for a
            // node identity and only when the operation needs it; ordinary
            // users and controller requests keep the existing hot path.
            if authz::node::node_name(identity.as_ref()).is_some() {
                let operation = if is_create {
                    admission::attributes::Operation::Create
                } else if is_update {
                    admission::attributes::Operation::Update
                } else {
                    admission::attributes::Operation::Delete
                };
                let old_object = if is_update || is_delete {
                    match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                        Ok(rest::GetOutcome::Found(object)) => Some(object),
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission: reading the existing object for NodeRestriction failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                } else {
                    None
                };
                match admission::node_restriction::validate(
                    &mut client,
                    identity.as_ref(),
                    operation,
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                    &info.namespace,
                    &info.name,
                    body_value.as_ref(),
                    old_object.as_ref(),
                )
                .await
                {
                    Ok(()) => {}
                    Err(admission::node_restriction::Error::Forbidden(message)) => {
                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                    }
                    Err(admission::node_restriction::Error::Lookup(error)) => {
                        warn!(path = %path_str, error = %error, "admission: NodeRestriction lookup failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            }

            // Group J: run the pure mutating admission registry before the
            // storage-backed admission stages. This preserves the existing
            // DefaultTolerationSeconds -> ServiceAccount defaulting order,
            // while making pure plugins extensible without another direct
            // listener call for each one.
            if let Some(body) = body_value.as_mut() {
                let operation = if is_create {
                    admission::attributes::Operation::Create
                } else {
                    admission::attributes::Operation::Update
                };
                run_pure_admission(&pure_admission, operation, &info, body);
            }

            // Group J: `StorageObjectInUseProtection` — mutating,
            // `CREATE` only. Add the standard PV/PVC/VAC protection
            // finalizer before any later admission stage observes the
            // candidate; nodecontroller removes it when deletion is safe.
            if is_create {
                if let Some(body) = body_value.as_mut() {
                    admission::storage_object_in_use_protection::mutate(
                        &info.api_group,
                        &info.resource,
                        &info.subresource,
                        body,
                    );
                }
            }

            // `ServiceAccount`'s validating and I/O-backed mutation step
            // follows the pure registry. Defaulting has already happened;
            // `quick_decision` now says whether a real ServiceAccount lookup
            // is needed to finish the plugin.
            if is_create {
                if let Some(pod) = body_value.as_mut() {
                    if admission::service_account::applies_to(&info.api_group, &info.resource, &info.subresource) {
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
                                        if let Err(error) = admission::service_account::validate_secret_references(&sa, pod) {
                                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                                        }
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

            // Group J: `DefaultIngressClass` — mutating, `CREATE` only.
            // Keep this after the pure mutators and before validators so
            // later admission sees the final Ingress candidate.
            if is_create {
                if let Some(ingress) = body_value.as_mut() {
                    if admission::default_ingress_class::applies_to(&info.api_group, &info.resource, &info.subresource) {
                        match rest::list(&mut client, None, "networking.k8s.io", "v1", "ingressclasses", None, "", "", 0, "").await {
                            Ok(rest::ListOutcome::Found(list)) => {
                                let classes = list["items"].as_array().cloned().unwrap_or_default();
                                admission::default_ingress_class::mutate(ingress, &classes);
                            }
                            Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                            Err(error) => {
                                warn!(path = %path_str, error = ?error, "admission: listing ingress classes failed");
                            }
                        }
                    }
                }
            }

            // Group J: `Priority` — resolve a Pod's named or global-default
            // PriorityClass on create, preserve the plugin-owned fields on
            // update, and reject competing global defaults.
            if is_create || is_update {
                let operation = if is_create {
                    admission::attributes::Operation::Create
                } else {
                    admission::attributes::Operation::Update
                };
                if let Some(object) = body_value.as_mut() {
                    if admission::priority::applies_to_pod(
                        operation,
                        &info.api_group,
                        &info.resource,
                        &info.subresource,
                    ) {
                        if is_update {
                            match rest::get(&mut client, None, "", "v1", "pods", namespace, &info.name).await {
                                Ok(rest::GetOutcome::Found(old_pod)) => {
                                    if let Err(error) = admission::priority::preserve_update_fields(object, &old_pod) {
                                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                                    }
                                }
                                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {}
                                Err(error) => {
                                    warn!(path = %path_str, error = ?error, "admission: reading the existing Pod for Priority failed");
                                    return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                                }
                            }
                        } else {
                            let class_name = object.pointer("/spec/priorityClassName").and_then(Value::as_str).unwrap_or("").to_string();
                            let named_class = if class_name.is_empty() {
                                None
                            } else {
                                match rest::get(&mut client, None, "scheduling.k8s.io", "v1", "priorityclasses", None, &class_name).await {
                                    Ok(rest::GetOutcome::Found(priority_class)) => Some(priority_class),
                                    Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                                        return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &format!("no PriorityClass with name {class_name} was found"))));
                                    }
                                    Err(error) => {
                                        warn!(path = %path_str, error = ?error, "admission: PriorityClass lookup failed");
                                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                                    }
                                }
                            };
                            let default_class = if named_class.is_none() {
                                match rest::list(&mut client, None, "scheduling.k8s.io", "v1", "priorityclasses", None, "", "", 0, "").await {
                                    Ok(rest::ListOutcome::Found(list)) => {
                                        let classes = list["items"].as_array().cloned().unwrap_or_default();
                                        admission::priority::select_default(&classes).cloned()
                                    }
                                    Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => None,
                                    Err(error) => {
                                        warn!(path = %path_str, error = ?error, "admission: listing PriorityClasses failed");
                                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                                    }
                                }
                            } else {
                                None
                            };
                            if let Err(error) = admission::priority::mutate_pod(object, named_class.as_ref(), default_class.as_ref()) {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                            }
                        }
                    } else if admission::priority::applies_to_priority_class(
                        operation,
                        &info.api_group,
                        &info.resource,
                        &info.subresource,
                    ) && object.pointer("/globalDefault").and_then(Value::as_bool) == Some(true)
                    {
                        match rest::list(&mut client, None, "scheduling.k8s.io", "v1", "priorityclasses", None, "", "", 0, "").await {
                            Ok(rest::ListOutcome::Found(list)) => {
                                let existing = list["items"].as_array().cloned().unwrap_or_default();
                                if let Some(error) = admission::priority::validate_priority_class(object, &existing) {
                                    return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                                }
                            }
                            Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {}
                            Err(error) => {
                                warn!(path = %path_str, error = ?error, "admission: listing PriorityClasses for validation failed");
                                return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                            }
                        }
                    }
                }
            }

            // Group J: `RuntimeClass` — mutating and validating, `CREATE`
            // only for ordinary Pods. The RuntimeClass plugin's informer
            // lookup is represented by this live read; the pure module owns
            // the same overhead validation/defaulting and scheduling merge
            // once the object is available.
            if is_create
                && admission::runtime_class::applies_to(
                    admission::attributes::Operation::Create,
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                )
            {
                if let Some(pod) = body_value.as_mut() {
                    let runtime_class_name = pod
                        .pointer("/spec/runtimeClassName")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let runtime_class = if let Some(runtime_class_name) = runtime_class_name {
                        match rest::get(
                            &mut client,
                            None,
                            "node.k8s.io",
                            "v1",
                            "runtimeclasses",
                            None,
                            &runtime_class_name,
                        )
                        .await
                        {
                            Ok(rest::GetOutcome::Found(runtime_class)) => Some(runtime_class),
                            Ok(rest::GetOutcome::ObjectNotFound)
                            | Ok(rest::GetOutcome::UnknownResource) => {
                                return Ok(json_response(
                                    StatusCode::FORBIDDEN,
                                    &admission_forbidden_status(
                                        &path_str,
                                        &format!(
                                            "pod rejected: RuntimeClass {runtime_class_name:?} not found"
                                        ),
                                    ),
                                ));
                            }
                            Err(error) => {
                                warn!(path = %path_str, error = ?error, "admission: RuntimeClass lookup failed");
                                return Ok(json_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    &internal_error_status(&path_str),
                                ));
                            }
                        }
                    } else {
                        None
                    };
                    if let Err(error) = admission::runtime_class::mutate_and_validate(pod, runtime_class.as_ref()) {
                        return Ok(json_response(
                            StatusCode::FORBIDDEN,
                            &admission_forbidden_status(&path_str, &error),
                        ));
                    }
                }
            }

            // Group J: `PodNodeSelector` — the namespace annotation form of
            // the upstream plugin. The annotation is an explicit opt-in, so
            // the live namespace read is harmless for ordinary namespaces.
            if is_create
                && admission::pod_node_selector::applies_to(
                    admission::attributes::Operation::Create,
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                )
            {
                if let Some(pod) = body_value.as_mut() {
                    match rest::get(
                        &mut client,
                        None,
                        "",
                        "v1",
                        "namespaces",
                        None,
                        namespace.unwrap_or(""),
                    )
                    .await
                    {
                        Ok(rest::GetOutcome::Found(namespace_object)) => {
                            let selector = namespace_object
                                .pointer("/metadata/annotations/scheduler.alpha.kubernetes.io~1node-selector")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if let Err(error) = admission::pod_node_selector::merge_namespace_selector(pod, selector) {
                                return Ok(json_response(
                                    StatusCode::FORBIDDEN,
                                    &admission_forbidden_status(&path_str, &error),
                                ));
                            }
                        }
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {}
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission: namespace lookup for PodNodeSelector failed");
                            return Ok(json_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &internal_error_status(&path_str),
                            ));
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

            // Group J: `PersistentVolumeClaimResize` — a PVC expansion is
            // allowed only for a bound claim whose unchanged StorageClass
            // explicitly permits volume expansion.
            if is_update
                && admission::pvc_resize::applies_to(
                    admission::attributes::Operation::Update,
                    &info.api_group,
                    &info.resource,
                    &info.subresource,
                )
            {
                if let Some(candidate) = body_value.as_ref() {
                    let old_pvc = match rest::get(&mut client, None, "", "v1", "persistentvolumeclaims", namespace, &info.name).await {
                        Ok(rest::GetOutcome::Found(old_pvc)) => old_pvc,
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => Value::Null,
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission: reading the existing PVC for resize failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    };
                    match rest::list(&mut client, None, "storage.k8s.io", "v1", "storageclasses", None, "", "", 0, "").await {
                        Ok(rest::ListOutcome::Found(list)) => {
                            let classes = list["items"].as_array().cloned().unwrap_or_default();
                            if let Err(error) = admission::pvc_resize::validate_resize(candidate, &old_pvc, &classes) {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                            }
                        }
                        Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => {
                            if let Err(error) = admission::pvc_resize::validate_resize(candidate, &old_pvc, &[]) {
                                return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &error)));
                            }
                        }
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission: listing StorageClasses for PVC resize failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                }
            }

            // Group J: storage-backed `MutatingAdmissionPolicy` bindings.
            // Apply policy mutations after built-in mutators and before
            // built-in validators inspect or account for the final
            // candidate. UPDATE supplies the existing object as `oldObject`;
            // CREATE has no old object. The policy module also enforces the
            // admission-configuration exemptions required to avoid locking
            // the API server out of its own policy storage.
            if is_create || is_update {
                let old_object = if is_update {
                    match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                        Ok(rest::GetOutcome::Found(object)) => Some(object),
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                        Err(error) => {
                            warn!(path = %path_str, error = %error, "admission: reading the existing object for MutatingAdmissionPolicy failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                } else {
                    None
                };
                if let Some(candidate) = body_value.take() {
                    match admission::mutating_admission_policy::mutate(
                        &mut client,
                        if is_create { "CREATE" } else { "UPDATE" },
                        &info.api_group,
                        &info.api_version,
                        &info.resource,
                        &info.subresource,
                        &info.namespace,
                        &info.name,
                        candidate,
                        old_object.as_ref(),
                        dry_run,
                        identity.as_ref(),
                    )
                    .await
                    {
                        Ok(mutated) => body_value = Some(mutated),
                        Err(error) => {
                            warn!(path = %path_str, error = %error, "admission: MutatingAdmissionPolicy evaluation failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
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
            // Populated for whichever evaluator applies, consumed after
            // `rest::create` actually succeeds below. Computing this here (before
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

            // Group J: storage-backed `ValidatingAdmissionPolicy` bindings.
            // Authorization must complete before admission, and CEL gets
            // the same candidate/old object pair that the write will use.
            if is_create || is_update || is_delete {
                let operation = if is_create {
                    "CREATE"
                } else if is_update {
                    "UPDATE"
                } else {
                    "DELETE"
                };
                let old_object = if is_update || is_delete {
                    match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                        Ok(rest::GetOutcome::Found(object)) => Some(object),
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission: reading the existing object for ValidatingAdmissionPolicy failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                } else {
                    None
                };
                match admission::policy_enforcement::validate(&mut client, operation, &info.api_group, &info.api_version, &info.resource, &info.subresource, &info.namespace, &info.name, body_value.as_ref(), old_object.as_ref(), dry_run, identity.as_ref()).await {
                    Ok(outcome) => {
                        record_admission_outcome(admission_metadata.as_ref(), &outcome);
                        if let Some(message) = outcome.denial {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                        }
                    }
                    Err(error) => {
                        warn!(path = %path_str, error = %error, "admission: ValidatingAdmissionPolicy evaluation failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
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

            // Group J: invoke configured mutating and validating webhooks
            // after built-in admission and authorization have produced the
            // candidate object, but before REST persists it. UPDATE and
            // DELETE need the current object as oldObject (and DELETE's
            // object); a missing object is left to REST to report NotFound.
            if is_create || is_update || is_delete {
                let old_object = if is_update || is_delete {
                    match rest::get(&mut client, None, &info.api_group, &info.api_version, &info.resource, namespace, &info.name).await {
                        Ok(rest::GetOutcome::Found(object)) => Some(object),
                        Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => None,
                        Err(e) => {
                            warn!(path = %path_str, error = ?e, "admission: reading the existing object for webhooks failed");
                            return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                        }
                    }
                } else {
                    None
                };
                let webhook_object = body_value.clone().or_else(|| old_object.clone());
                if let Some(webhook_object) = webhook_object {
                    let operation = if is_create {
                        admission::attributes::Operation::Create
                    } else if is_update {
                        admission::attributes::Operation::Update
                    } else {
                        admission::attributes::Operation::Delete
                    };
                    match admission::webhook::admit(
                        &mut client,
                        operation,
                        &info.api_group,
                        &info.api_version,
                        &info.resource,
                        &info.subresource,
                        &info.namespace,
                        &info.name,
                        webhook_object,
                        old_object,
                        identity.as_ref(),
                        dry_run,
                    )
                    .await
                    {
                        Ok(admission::webhook::Outcome::Allowed(admitted)) => {
                            if is_create || is_update {
                                body_value = Some(admitted);
                            }
                        }
                        Ok(admission::webhook::Outcome::Denied(message)) => {
                            return Ok(json_response(StatusCode::FORBIDDEN, &admission_forbidden_status(&path_str, &message)));
                        }
                        Err(error) => {
                            warn!(path = %path_str, error = ?error, "admission webhook invocation failed");
                            return Ok(admission_webhook_error_response(&path_str, &error));
                        }
                    }
                }
            }

            // Built-in resources have a real cache registered at startup;
            // dynamically discovered CRD resources are registered by the
            // CRD lifecycle reconciler and can still be registered lazily
            // by the first watch if startup discovery has not caught up.
            // Shared by both verbs below; `rest::list`'s own doc
            // comment covers why an unsynced cache is safe to pass here
            // too (it just falls through, same as `None`).
            let resource_cache = cache_registry.get(&info.api_group, &info.api_version, &info.resource);
            let resource_cache = resource_cache.as_ref();

            if is_get {
                match rest::get_at_revision(&mut client, resource_cache, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, resource_version_query(&query)).await {
                    Ok(rest::GetOutcome::Found(object)) => {
                        let body = if wants_table {
                            crate::codec::table::convert_to_table_for_resource_with_crd_columns(&info.api_group, &info.api_version, &info.resource, crd_printer_columns.as_deref(), &object)
                        } else if wants_partial_metadata {
                            crate::codec::partial_metadata::object(&object)
                        } else {
                            object
                        };
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
                if !info.field_selector.is_empty() {
                    match crate::cacher::selector::parse_field_selector(&info.field_selector) {
                        Ok(requirements) => {
                            if let Err(error) = crate::cacher::selector::validate_field_selector(&info.api_group, &info.resource, &requirements) {
                                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string())));
                            }
                        }
                        Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &error.to_string()))),
                    }
                }
                match rest::list_at_revision(&mut client, resource_cache, &info.api_group, &info.api_version, &info.resource, namespace, &info.label_selector, &info.field_selector, info.limit, &info.continue_token, resource_version_query(&query)).await {
                    Ok(rest::ListOutcome::Found(list)) => {
                        let body = if wants_table {
                            crate::codec::table::convert_to_table_for_resource_with_crd_columns(&info.api_group, &info.api_version, &info.resource, crd_printer_columns.as_deref(), &list)
                        } else if wants_partial_metadata {
                            crate::codec::partial_metadata::list(&list)
                        } else {
                            list
                        };
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
                match rest::create_with_options_and_manager(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &body_value, dry_run, request_field_manager.as_deref()).await {
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
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.name or metadata.generateName is required")));
                    }
                    Ok(rest::CreateOutcome::NamespaceMismatch) => {
                        return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, "metadata.namespace does not match the request URL")));
                    }
                    Ok(rest::CreateOutcome::AlreadyExists) => return Ok(json_response(StatusCode::CONFLICT, &conflict_status(&path_str))),
                    Ok(rest::CreateOutcome::Invalid(violations)) => return Ok(json_response(StatusCode::UNPROCESSABLE_ENTITY, &invalid_status(&path_str, &violations))),
                    Ok(rest::CreateOutcome::UnsupportedForCrd) => {
                        return Ok(json_response(StatusCode::NOT_IMPLEMENTED, &bad_request_status(&path_str, "this resource has no usable structural schema")));
                    }
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::create failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            } else if is_update {
                let body_value = body_value.expect("body_value is Some whenever is_update is true (has_body covers it)");
                match rest::update_with_options_and_manager(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, &body_value, dry_run, request_field_manager.as_deref()).await {
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
                let preconditions = match delete_preconditions(delete_options.as_ref()) {
                    Ok(value) => value,
                    Err(detail) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, detail))),
                };
                match rest::delete_with_options(&mut client, &info.api_group, &info.api_version, &info.resource, namespace, &info.name, preconditions.as_ref(), dry_run).await {
                    Ok(rest::DeleteOutcome::Deleted(object)) => return Ok(json_response(StatusCode::OK, &object)),
                    Ok(rest::DeleteOutcome::ObjectNotFound) | Ok(rest::DeleteOutcome::UnknownResource) => {
                        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
                    }
                    Ok(rest::DeleteOutcome::PreconditionFailed) => {
                        return Ok(json_response(StatusCode::CONFLICT, &precondition_failed_status(&path_str)));
                    }
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "rest::delete failed");
                        return Ok(json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(&path_str)));
                    }
                }
            }
        }
        // No nodestore connection at all (failed at startup, or not yet
        // reconnected) — handled by the real unavailable/not-found response below.
    }

    // Group D/E: real `WATCH`, served purely from an already-registered
    // `cacher::CacheRegistry` cache. A live cache already holds
    // everything the read side of this handler needs (a snapshot to
    // replay from, a live event subscription). If a resource has no
    // registered cache, the handler returns a real Kubernetes error below
    // rather than claiming a successful watch this build cannot serve.
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
        // Group K: an already-registered cache first (unchanged), else —
        // only when the static table doesn't know this resource at all —
        // a live check against the dynamic CRD registry, lazily spawning
        // a cache for it right now on this, its first-ever watch request
        // (`cacher::registry::CacheRegistry::spawn` is callable at any
        // time, not just at boot — see its own doc comment). Only
        // a resource the static table has never heard of falls through to
        // the dynamic check, so this never masks a genuine 404 as "maybe
        // a CRD." Proactive CRD lifecycle reconciliation is started with
        // the listener's built-in CRD cache above; this lazy path remains
        // only as a bounded startup-race fallback for a CRD that is
        // discovered before that reconciler has registered its cache.
        let cache_and_kind: Option<(
            crate::cacher::store::SharedCache,
            String,
            Option<crate::apiextensions::registry::ConversionWebhook>,
        )> = if let Some(cache) = cache_registry.get(&info.api_group, &info.api_version, &info.resource) {
            if let Some(kind) = rest::resolve_kind(&info.api_group, &info.api_version, &info.resource) {
                Some((cache, kind.to_string(), None))
            } else if let Some(mut client) = storage.clone() {
                match rest::resolve_dynamic_resource(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                    Ok(Some(resource)) => Some((cache, resource.kind, resource.conversion_webhook)),
                    Ok(None) => None,
                    Err(e) => {
                        warn!(path = %path_str, error = ?e, "watch: resolving the registered CRD-defined resource failed");
                        None
                    }
                }
            } else {
                None
            }
        } else if rest::resolve_kind(&info.api_group, &info.api_version, &info.resource).is_some() {
            None
        } else if let Some(mut client) = storage.clone() {
            match rest::resolve_dynamic_resource(&mut client, &info.api_group, &info.api_version, &info.resource).await {
                Ok(Some(resource)) => {
                    let cache = cache_registry.spawn(client, &info.api_group, &info.api_version, &info.resource);
                    Some((cache, resource.kind, resource.conversion_webhook))
                }
                Ok(None) => None,
                Err(e) => {
                    warn!(path = %path_str, error = ?e, "watch: resolving a possible CRD-defined resource failed");
                    None
                }
            }
        } else {
            None
        };

        if let Some((cache, kind, conversion_webhook)) = cache_and_kind {
            if !cache.has_synced() {
                if tokio::time::timeout(std::time::Duration::from_secs(30), cache.wait_until_synced()).await.is_err() {
                    warn!(path = %path_str, "watch: cache did not complete its initial LIST before the startup wait expired");
                    return Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(&path_str, "watch cache is not synchronized yet")));
                }
            }
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
            if let Err(e) = crate::cacher::selector::validate_field_selector(&info.api_group, &info.resource, &field_reqs) {
                return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, &e.to_string())));
            }
            let start_revision = resource_version_query(&query);
            let watch_options = match watch_options_query(&query) {
                Ok(options) => options,
                Err(error) => return Ok(json_response(StatusCode::BAD_REQUEST, &bad_request_status(&path_str, error))),
            };
            // Newer client-go informers use the streaming-list form of WATCH:
            // they request the current objects as synthetic ADDED events and
            // do not consider the informer synchronized until the server
            // sends a BOOKMARK annotated `k8s.io/initial-events-end=true`.
            // Take the cache snapshot before subscribing; `watch_from` then
            // replays any event racing that snapshot, preserving the normal
            // LIST-then-WATCH handoff without a gap.
            let initial_events = if watch_options.send_initial_events {
                let (entries, revision) = cache.list();
                let prefix = crate::storage::keys::list_prefix(&info.api_group, &info.resource, Some(&info.namespace)).into_bytes();
                let events = entries
                    .into_iter()
                    .filter(|(key, _)| key.starts_with(&prefix))
                    .map(|(key, entry)| crate::cacher::store::WatchEvent {
                        kind: crate::cacher::store::EventKind::Added,
                        key,
                        value: entry.value,
                        revision,
                    })
                    .collect();
                Some((events, revision))
            } else {
                None
            };
            let watch_start_revision = initial_events.as_ref().map(|(_, revision)| *revision).unwrap_or(start_revision);
            let watch_result = if initial_events.is_some() {
                cache.watch_from_snapshot(watch_start_revision)
            } else {
                cache.watch_from(watch_start_revision)
            };
            match watch_result {
                Ok((replay, rx)) => {
                    let group_version = if info.api_group.is_empty() { info.api_version.clone() } else { format!("{}/{}", info.api_group, info.api_version) };
                    let body = if initial_events.is_some() {
                        watch_response_body_with_initial_events(
                            replay,
                            rx,
                            kind,
                            group_version,
                            label_reqs,
                            field_reqs,
                            storage.clone(),
                            info.api_group.clone(),
                            info.resource.clone(),
                            info.api_version.clone(),
                            wants_partial_metadata,
                            watch_options.allow_watch_bookmarks,
                            watch_options.timeout,
                            conversion_webhook,
                            initial_events,
                        )
                    } else {
                        watch_response_body(
                            replay,
                            rx,
                            kind,
                            group_version,
                            label_reqs,
                            field_reqs,
                            storage.clone(),
                            info.api_group.clone(),
                            info.resource.clone(),
                            info.api_version.clone(),
                            wants_partial_metadata,
                            watch_options.allow_watch_bookmarks,
                            watch_options.timeout,
                            conversion_webhook,
                        )
                    };
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
        // through to the real not-found response below.
    }

    // A resource-shaped request that reached this point targeted a verb or
    // subresource this server does not serve. Real kube-apiserver returns a
    // Kubernetes NotFound status for an unknown subresource.
    if info.is_resource_request {
        return Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)));
    }

    // Unknown non-resource paths are also real API errors. The old bring-up
    // echo made a typo look like a successful request and was incompatible
    // with kubectl/client-go error handling.
    Ok(json_response(StatusCode::NOT_FOUND, &not_found_status(&path_str)))
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
/// method, headers minus [`HOP_BY_HOP_HEADERS`] and untrusted front-proxy
/// headers, body — through (`proxy::http_client::relay`). When configured,
/// the trusted front-proxy certificate and authenticated identity headers
/// are added in the same way as real kube-aggregator.
///
/// A cached `Available: False` condition (`aggregator::reconcile`'s own
/// periodic write, `availability::cached_available`) short-circuits
/// straight to `503` before any of the Service/`EndpointSlice` I/O below
/// — a known-broken backend fails fast without paying for a fetch this
/// build already knows the answer to. `Available: True` or no cached
/// condition yet both fall through to the full check unchanged (the
/// backing Service still has to be fetched either way, to resolve the
/// actual dial target — this only ever saves the *negative* path).
async fn aggregate_proxy(
    req: Request<Incoming>,
    method: &str,
    api_service: &serde_json::Value,
    mut client: StorageClient,
    path_str: &str,
    query: &str,
    identity: Option<&crate::authn::x509::Identity>,
    proxy_identity: Option<&crate::aggregator::client_tls::ClientIdentity>,
) -> Response<BoxedBody> {
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
    let client_config = match aggregator::client_tls::build_client_config_with_identity(ca_bundle_pem.as_deref(), insecure_skip_tls_verify, proxy_identity) {
        Ok(cfg) => std::sync::Arc::new(cfg),
        Err(e) => {
            warn!(path = %path_str, error = ?e, "aggregation: building the backend TLS client config failed");
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
        }
    };

    if is_connection_upgrade(req.headers()) {
        let auth_headers = auth_proxy_headers(identity, proxy_identity.is_some());
        return match proxy::http_client::upgrade_with_headers(req, &target, client_config, Some(&auth_headers)).await {
            Ok(resp) => resp,
            Err(e) => {
                warn!(path = %path_str, host = %target.host, error = ?e, "aggregation: dialing the upgraded backend failed");
                json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, &e.to_string()))
            }
        };
    }

    let headers = aggregation_proxy_headers(req.headers(), identity, proxy_identity.is_some());
    let body = match read_body_bytes(req).await {
        Ok(b) => b,
        Err(e) => {
            warn!(path = %path_str, error = ?e, "aggregation: reading the request body failed");
            return body_read_error_response(path_str, &e);
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

fn is_auth_proxy_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("x-remote-user")
        || name.eq_ignore_ascii_case("x-remote-group")
        || name.eq_ignore_ascii_case("x-remote-uid")
        || name.len() >= "x-remote-extra-".len()
            && name[.."x-remote-extra-".len()].eq_ignore_ascii_case("x-remote-extra-")
}

fn is_connection_upgrade(headers: &http::HeaderMap) -> bool {
    let connection_upgrade = headers
        .get(http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|token| token.trim().eq_ignore_ascii_case("upgrade")));
    connection_upgrade && headers.contains_key(http::header::UPGRADE)
}

/// Matches client-go's `headerKeyEscape`: HTTP field names cannot contain
/// arbitrary user-extra keys (the standard credential-id key contains `/`),
/// so escape non-token bytes as uppercase percent-encoded octets. The
/// request-header authenticator reverses this with `url.PathUnescape`.
fn escape_auth_proxy_extra_key(key: &str) -> String {
    let mut escaped = String::with_capacity(key.len());
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&'*+-.^_`|~".contains(&byte) {
            escaped.push(byte as char);
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    escaped
}

fn aggregation_proxy_headers(
    incoming: &http::HeaderMap,
    identity: Option<&crate::authn::x509::Identity>,
    add_identity: bool,
) -> Vec<(String, String)> {
    let mut headers = incoming
        .iter()
        .filter(|(name, _)| !HOP_BY_HOP_HEADERS.contains(&name.as_str().to_ascii_lowercase().as_str()))
        .filter(|(name, _)| !is_auth_proxy_header(name.as_str()))
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str().to_string(), value.to_string())))
        .collect::<Vec<_>>();
    headers.extend(auth_proxy_headers(identity, add_identity));
    headers
}

fn auth_proxy_headers(
    identity: Option<&crate::authn::x509::Identity>,
    add_identity: bool,
) -> Vec<(String, String)> {
    if !add_identity {
        return Vec::new();
    }
    let anonymous_groups = [UNAUTHENTICATED_GROUP.to_string()];
    let (user, groups, uid) = match identity {
        Some(identity) => (identity.name.as_str(), identity.groups.as_slice(), identity.uid.as_deref()),
        None => (ANONYMOUS_USERNAME, &anonymous_groups[..], None),
    };
    let mut headers = vec![("X-Remote-User".to_string(), user.to_string())];
    headers.extend(groups.iter().cloned().map(|group| ("X-Remote-Group".to_string(), group)));
    if let Some(uid) = uid {
        headers.push(("X-Remote-Uid".to_string(), uid.to_string()));
    }
    let mut extra = identity.map(|identity| identity.extra.clone()).unwrap_or_default();
    if let Some(identity) = identity {
        if !identity.credential_id.0.is_empty() && !identity.credential_id.1.is_empty() {
            extra.entry(identity.credential_id.0.clone()).or_insert_with(|| identity.credential_id.1.clone());
        }
    }
    for (name, values) in extra {
        let header_name = format!("X-Remote-Extra-{}", escape_auth_proxy_extra_key(&name));
        headers.extend(values.into_iter().map(|value| (header_name.clone(), value)));
    }
    headers
}

fn is_proxy_request(info: &path::RequestInfo) -> bool {
    info.verb == "proxy" || info.subresource == "proxy"
}

/// Returns the path after the `proxy` marker.  `RequestInfo.parts` has
/// already removed the API prefix, group/version, and optional namespace,
/// so this handles both supported Kubernetes forms:
/// `.../{resource}/{name}/proxy/{path}` and
/// `.../proxy/{resource}/{name}/{path}`.
fn proxy_suffix(info: &path::RequestInfo) -> String {
    let start = if info.verb == "proxy" {
        2
    } else {
        info.parts.iter().position(|part| part == "proxy").map_or(info.parts.len(), |index| index + 1)
    };
    let suffix = info.parts.get(start..).map(|parts| parts.join("/")).unwrap_or_default();
    if suffix.is_empty() { "/".to_string() } else { format!("/{suffix}") }
}

/// Group N's core node/service proxy dispatch.  The object and EndpointSlice
/// reads are intentionally performed before consuming the request body so an
/// invalid or unavailable target returns a normal Kubernetes Status response.
async fn proxy_resource(
    req: Request<Incoming>,
    storage: Option<StorageClient>,
    info: &path::RequestInfo,
    method: &str,
    path_str: &str,
    query: &str,
    identity: &Option<crate::authn::x509::Identity>,
    enforce_rbac: bool,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Response<BoxedBody> {
    let Some(mut client) = storage else {
        return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
    };

    if enforce_rbac {
        let (user_name, user_groups): (&str, Vec<String>) = match identity {
            Some(id) => (id.name.as_str(), id.groups.clone()),
            None => (ANONYMOUS_USERNAME, vec![UNAUTHENTICATED_GROUP.to_string()]),
        };
        let resolved = authz::resolve::rules_for(&mut client, user_name, &user_groups, &info.namespace).await;
        let subresource = if info.verb == "proxy" { "proxy" } else { info.subresource.as_str() };
        let attrs = authz::rbac::RequestAttributes {
            is_resource_request: true,
            verb: &info.verb,
            api_group: &info.api_group,
            resource: &info.resource,
            subresource,
            name: &info.name,
            path: path_str,
        };
        if !authz::rbac::rules_allow(&attrs, &resolved.rules) {
            return json_response(StatusCode::FORBIDDEN, &forbidden_status(path_str, user_name));
        }
    }

    let suffix = proxy_suffix(info);
    let target = if info.resource == "nodes" {
        let node = match rest::get(&mut client, None, "", "v1", "nodes", None, &info.name).await {
            Ok(rest::GetOutcome::Found(node)) => node,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return json_response(StatusCode::NOT_FOUND, &not_found_status(path_str));
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "proxy: fetching the node failed");
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
            }
        };
        match proxy::node_proxy::target(&node, &suffix, query) {
            Ok(target) => target,
            Err(proxy::node_proxy::Error::NoNodeAddress) => {
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
            }
        }
    } else {
        if info.namespace.is_empty() {
            return json_response(StatusCode::BAD_REQUEST, &bad_request_status(path_str, "service proxy requires a namespace"));
        }
        let (service_name, _) = proxy::service_proxy::split_name(&info.name);
        let service = match rest::get(&mut client, None, "", "v1", "services", Some(&info.namespace), service_name).await {
            Ok(rest::GetOutcome::Found(service)) => service,
            Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                return json_response(StatusCode::NOT_FOUND, &not_found_status(path_str));
            }
            Err(error) => {
                warn!(path = %path_str, error = ?error, "proxy: fetching the Service failed");
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
            }
        };
        let endpoint_slices = match rest::list(&mut client, None, "discovery.k8s.io", "v1", "endpointslices", Some(&info.namespace), &format!("kubernetes.io/service-name={service_name}"), "", 0, "").await {
            Ok(rest::ListOutcome::Found(list)) => list.get("items").and_then(serde_json::Value::as_array).cloned().unwrap_or_default(),
            Ok(rest::ListOutcome::UnknownResource) | Ok(rest::ListOutcome::InvalidContinueToken) => Vec::new(),
            Err(error) => {
                warn!(path = %path_str, error = ?error, "proxy: listing EndpointSlices failed");
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
            }
        };
        match proxy::service_proxy::target(&service, &endpoint_slices, &info.name, &suffix, query) {
            Ok(target) => target,
            Err(proxy::service_proxy::Error::MissingPort
                | proxy::service_proxy::Error::InvalidPort(_)
                | proxy::service_proxy::Error::UnsupportedProtocol(_)) =>
            {
                return json_response(StatusCode::BAD_REQUEST, &bad_request_status(path_str, "the requested Service port does not exist"));
            }
            Err(proxy::service_proxy::Error::NoClusterIpOrEndpoint) => {
                return json_response(StatusCode::SERVICE_UNAVAILABLE, &service_unavailable_status(path_str, "Service has no ready endpoints or ClusterIP"));
            }
        }
    };

    let headers = req
        .headers()
        .iter()
        .filter(|(name, _)| !HOP_BY_HOP_HEADERS.contains(&name.as_str()))
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str().to_string(), value.to_string())))
        .collect::<Vec<_>>();
    let body = match read_body_bytes(req).await {
        Ok(body) => body,
        Err(error) => {
            warn!(path = %path_str, error = ?error, "proxy: reading the request body failed");
            return body_read_error_response(path_str, &error);
        }
    };

    let client_config = if target.scheme == "https" && info.resource == "services" {
        match crate::proxy::client_tls::build_client_config(None) {
            Ok(config) => std::sync::Arc::new(config),
            Err(error) => {
                warn!(path = %path_str, error = ?error, "proxy: building the Service TLS client config failed");
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str));
            }
        }
    } else {
        // Node proxies use the kubelet client configuration built at
        // listener startup.  Plain HTTP Service targets ignore it.
        kubelet_tls
    };

    match proxy::http_client::relay(&target, client_config, method, &headers, body).await {
        Ok(response) => response,
        Err(error) => {
            warn!(path = %path_str, host = %target.host, error = ?error, "proxy: dialing the backend failed");
            json_response(StatusCode::BAD_GATEWAY, &bad_gateway_status(path_str, &error.to_string()))
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
        assert_eq!(discovery_content_type(&parts("/api"), Some(accept)), AGGREGATED_DISCOVERY_CONTENT_TYPE);
        assert_eq!(discovery_content_type(&parts("/api/v1"), Some(accept)), "application/json");
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
    fn openapi_v2_serves_a_swagger_document() {
        let route = route_discovery(&parts("/openapi/v2"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else { panic!("expected Found") };
        assert_eq!(doc["swagger"], "2.0");
        assert!(doc["definitions"].as_object().is_some_and(|definitions| definitions.contains_key("io.k8s.api.core.v1.Pod")));
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
    fn oversized_body_status_uses_the_real_http_error_shape() {
        let status = request_entity_too_large_status("/api/v1/configmaps", 8192);
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "RequestEntityTooLarge");
        assert_eq!(status["code"], 413);
        assert!(status["message"].as_str().unwrap().contains("8192-byte limit"));
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
    fn dry_run_query_accepts_only_all() {
        assert_eq!(dry_run_query("dryRun=All").unwrap(), true);
        assert_eq!(dry_run_query("fieldManager=test").unwrap(), false);
        assert_eq!(dry_run_query("dryRun=Unknown").unwrap_err(), "dryRun must be All");
    }

    #[test]
    fn authorization_reviews_bypass_resource_enforcement() {
        let sar = path::parse(
            "POST",
            "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
            "",
        );
        let self_review = path::parse(
            "POST",
            "/apis/authentication.k8s.io/v1/selfsubjectreviews",
            "",
        );
        let pods = path::parse("PATCH", "/api/v1/namespaces/default/pods/p1", "");

        assert!(is_authorization_review(&sar));
        assert!(is_authorization_review(&self_review));
        assert!(!is_authorization_review(&pods));
    }

    #[test]
    fn an_authorization_webhook_allow_short_circuits_local_resource_authorization() {
        let pod = path::parse("GET", "/api/v1/namespaces/default/pods/p1", "");
        let healthz = path::parse("GET", "/healthz", "");
        let review = path::parse(
            "POST",
            "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
            "",
        );

        assert!(!should_run_local_authorization(&pod, true, true));
        assert!(should_run_local_authorization(&pod, true, false));
        assert!(should_run_local_authorization(&healthz, true, false));
        assert!(!should_run_local_authorization(&healthz, true, true));
        assert!(!should_run_local_authorization(&review, true, false));
        assert!(!should_run_local_authorization(&pod, false, false));
    }

    #[test]
    fn aggregation_proxy_headers_replace_caller_supplied_identity_headers() {
        let mut incoming = http::HeaderMap::new();
        incoming.insert("X-Remote-User", "attacker".parse().unwrap());
        incoming.append("X-Remote-Group", "untrusted".parse().unwrap());
        incoming.insert("X-Remote-Extra-tenant", "untrusted".parse().unwrap());
        incoming.insert("X-Trace-Id", "trace-1".parse().unwrap());
        let identity = crate::authn::x509::Identity {
            name: "alice".to_string(),
            groups: vec!["developers".to_string(), "system:authenticated".to_string()],
            uid: Some("uid-1".to_string()),
            extra: Default::default(),
            credential_id: ("authentication.kubernetes.io/credential-id".to_string(), vec!["X509SHA256=abc".to_string()]),
        };

        let headers = aggregation_proxy_headers(&incoming, Some(&identity), true);
        assert_eq!(headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("x-remote-user")).map(|(_, value)| value.as_str()).collect::<Vec<_>>(), ["alice"]);
        assert_eq!(headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("x-remote-group")).map(|(_, value)| value.as_str()).collect::<Vec<_>>(), ["developers", "system:authenticated"]);
        assert_eq!(headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("x-remote-uid")).map(|(_, value)| value.as_str()), Some("uid-1"));
        assert_eq!(headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("x-remote-extra-authentication.kubernetes.io%2Fcredential-id")).map(|(_, value)| value.as_str()), Some("X509SHA256=abc"));
        assert!(headers.iter().any(|(name, value)| name == "x-trace-id" && value == "trace-1"));
        assert!(!headers.iter().any(|(_, value)| value == "attacker" || value == "untrusted"));
    }

    #[test]
    fn aggregation_proxy_headers_strip_identity_headers_without_a_proxy_identity() {
        let mut incoming = http::HeaderMap::new();
        incoming.insert("X-Remote-User", "attacker".parse().unwrap());
        incoming.insert("X-Remote-Group", "untrusted".parse().unwrap());
        let headers = aggregation_proxy_headers(&incoming, None, false);
        assert!(headers.is_empty());
    }

    #[test]
    fn aggregation_proxy_headers_do_not_emit_an_empty_credential_extra() {
        let identity = crate::authn::x509::Identity {
            name: "system:serviceaccount:default:builder".to_string(),
            groups: vec!["system:serviceaccounts".to_string()],
            uid: None,
            extra: Default::default(),
            credential_id: (String::new(), Vec::new()),
        };
        let headers = aggregation_proxy_headers(&http::HeaderMap::new(), Some(&identity), true);
        assert_eq!(headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("x-remote-user")).count(), 1);
        assert!(!headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("x-remote-extra-")));
    }

    #[test]
    fn connection_upgrade_requires_upgrade_token_and_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::CONNECTION, "keep-alive, Upgrade".parse().unwrap());
        headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
        assert!(is_connection_upgrade(&headers));

        headers.insert(http::header::CONNECTION, "keep-alive".parse().unwrap());
        assert!(!is_connection_upgrade(&headers));

        headers.insert(http::header::CONNECTION, "Upgrade".parse().unwrap());
        headers.remove(http::header::UPGRADE);
        assert!(!is_connection_upgrade(&headers));
    }

    #[test]
    fn delete_preconditions_decode_resource_version_and_uid() {
        let value = serde_json::json!({"preconditions": {"resourceVersion": "7", "uid": "abc"}});
        assert_eq!(
            delete_preconditions(Some(&value)).unwrap(),
            Some(rest::DeletePreconditions { resource_version: Some("7".to_string()), uid: Some("abc".to_string()) })
        );
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
        assert_eq!(status["details"]["causes"][0]["field"], "spec.containers");
        assert_eq!(status["details"]["causes"][0]["reason"], "FieldValueRequired");
        assert_eq!(status["details"]["causes"][1]["field"], "spec.foo");
        assert_eq!(status["details"]["causes"][1]["reason"], "FieldValueInvalid");
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
    fn watch_options_parse_bookmarks_and_timeout() {
        let options = watch_options_query("watch=true&allowWatchBookmarks=true&sendInitialEvents=true&timeoutSeconds=7").unwrap();
        assert!(options.allow_watch_bookmarks);
        assert!(options.send_initial_events);
        assert_eq!(options.timeout, Some(std::time::Duration::from_secs(7)));
        assert_eq!(watch_options_query("allowWatchBookmarks=0&sendInitialEvents=0&timeoutSeconds=0").unwrap(), WatchOptions::default());
        assert!(watch_options_query("allowWatchBookmarks=maybe").is_err());
        assert!(watch_options_query("sendInitialEvents=maybe").is_err());
        assert!(watch_options_query("timeoutSeconds=-1").is_err());
        assert!(watch_options_query("timeoutSeconds=not-a-number").is_err());
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
        let frame = encode_watch_event(&event, "Pod", "v1", None, "", "pods", "v1", false, false).expect("Bookmark always converts").expect("Bookmark conversion never fails");
        let bytes = frame.into_data().unwrap();
        assert!(bytes.ends_with(b"\n"));
        let parsed: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(parsed["type"], "BOOKMARK");
        assert_eq!(parsed["object"]["kind"], "Pod");
    }

    #[test]
    fn encode_watch_event_marks_the_end_of_streaming_list_initial_events() {
        let event = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Bookmark,
            key: Vec::new(),
            value: Vec::new(),
            revision: 9,
        };
        let frame = encode_watch_event(&event, "Pod", "v1", None, "", "pods", "v1", false, true)
            .expect("Bookmark always converts")
            .expect("Bookmark conversion never fails");
        let bytes = frame.into_data().unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(parsed["type"], "BOOKMARK");
        assert_eq!(parsed["object"]["metadata"]["annotations"]["k8s.io/initial-events-end"], "true");
    }

    #[test]
    fn encode_watch_event_skips_a_deleted_event_with_no_retained_value() {
        let event = crate::cacher::store::WatchEvent { kind: crate::cacher::store::EventKind::Deleted, key: b"k".to_vec(), value: Vec::new(), revision: 9 };
        assert!(encode_watch_event(&event, "Pod", "v1", None, "", "pods", "v1", false, false).is_none());
    }

    #[test]
    fn encode_watch_event_converts_objects_to_partial_metadata_when_requested() {
        let event = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Added,
            key: b"k".to_vec(),
            value: envelope_for("default", serde_json::json!({"app": "demo"})),
            revision: 9,
        };
        let frame = encode_watch_event(&event, "Namespace", "v1", None, "", "namespaces", "v1", true, false)
            .expect("Added events always convert")
            .expect("the test envelope must decode");
        let bytes = frame.into_data().unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(parsed["object"]["apiVersion"], "meta.k8s.io/v1");
        assert_eq!(parsed["object"]["kind"], "PartialObjectMetadata");
        assert_eq!(parsed["object"]["metadata"]["name"], "default");
        assert!(parsed["object"].get("spec").is_none());
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

        let body = watch_response_body(
            replay,
            rx,
            "Namespace".to_string(),
            "v1".to_string(),
            Vec::new(),
            Vec::new(),
            None,
            String::new(),
            "namespaces".to_string(),
            "v1".to_string(),
            false,
            true,
            None,
            None,
        );
        let collected = body.collect().await.unwrap().to_bytes();
        let text = String::from_utf8(collected.to_vec()).unwrap();
        assert_eq!(text.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["type"], "ADDED");
    }

    #[tokio::test]
    async fn watch_response_body_honors_bookmark_negotiation_and_timeout() {
        use http_body_util::BodyExt;

        let bookmark = crate::cacher::store::WatchEvent { kind: crate::cacher::store::EventKind::Bookmark, key: Vec::new(), value: Vec::new(), revision: 9 };
        let (_, rx) = {
            let cache = crate::cacher::store::WatchCache::new(vec![], 0, 16, 16);
            cache.watch_from(0).unwrap()
        };
        let body = watch_response_body(
            vec![bookmark.clone()],
            rx,
            "Namespace".to_string(),
            "v1".to_string(),
            Vec::new(),
            Vec::new(),
            None,
            String::new(),
            "namespaces".to_string(),
            "v1".to_string(),
            false,
            false,
            None,
            None,
        );
        let bytes = body.collect().await.unwrap().to_bytes();
        assert!(bytes.is_empty(), "bookmarks must be opt-in");

        let (_, rx) = {
            let cache = crate::cacher::store::WatchCache::new(vec![], 0, 16, 16);
            cache.watch_from(0).unwrap()
        };
        let body = watch_response_body(
            Vec::new(),
            rx,
            "Namespace".to_string(),
            "v1".to_string(),
            Vec::new(),
            Vec::new(),
            None,
            String::new(),
            "namespaces".to_string(),
            "v1".to_string(),
            false,
            false,
            Some(std::time::Duration::from_millis(10)),
            None,
        );
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(1), body.collect()).await.unwrap().unwrap().to_bytes();
        assert!(bytes.is_empty(), "an idle watch must terminate at timeoutSeconds");
    }

    #[tokio::test]
    async fn watch_response_body_sends_streaming_list_initial_events_end_bookmark() {
        use http_body_util::BodyExt;

        let initial = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Added,
            key: b"/registry/namespaces/default".to_vec(),
            value: envelope_for("default", serde_json::json!({})),
            revision: 5,
        };
        let cache = crate::cacher::store::WatchCache::new(vec![], 5, 16, 16);
        let (_, rx) = cache.watch_from(5).unwrap();
        drop(cache);

        let body = watch_response_body_with_initial_events(
            Vec::new(),
            rx,
            "Namespace".to_string(),
            "v1".to_string(),
            Vec::new(),
            Vec::new(),
            None,
            String::new(),
            "namespaces".to_string(),
            "v1".to_string(),
            false,
            true,
            None,
            None,
            Some((vec![initial], 5)),
        );
        let bytes = body.collect().await.unwrap().to_bytes();
        let lines: Vec<serde_json::Value> = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["type"], "ADDED");
        assert_eq!(lines[1]["type"], "BOOKMARK");
        assert_eq!(lines[1]["object"]["metadata"]["resourceVersion"], "5");
        assert_eq!(lines[1]["object"]["metadata"]["annotations"]["k8s.io/initial-events-end"], "true");
    }

    fn test_peer() -> SocketAddr {
        "10.0.0.7:54321".parse().unwrap()
    }

    #[test]
    fn build_audit_event_carries_the_real_request_shape_for_an_anonymous_user() {
        let event = build_audit_event("GET", "/api/v1/namespaces/default/pods/web-1", "", None, None, &test_peer(), 200, &BTreeMap::new());
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
        let identity = crate::authn::x509::Identity { name: "alice".to_string(), groups: vec!["developers".to_string()], uid: None, extra: Default::default(), credential_id: (String::new(), Vec::new()) };
        let event = build_audit_event("GET", "/api/v1/pods", "watch=true", None, Some(&identity), &test_peer(), 200, &BTreeMap::new());
        assert_eq!(event["user"]["username"], "alice");
        assert_eq!(event["user"]["groups"], serde_json::json!(["developers"]));
        assert_eq!(event["verb"], "watch");
        assert_eq!(event["requestURI"], "/api/v1/pods?watch=true");
    }

    #[test]
    fn build_audit_event_has_no_object_ref_for_a_non_resource_request() {
        let event = build_audit_event("GET", "/version", "", None, None, &test_peer(), 200, &BTreeMap::new());
        assert!(event.get("objectRef").is_none());
    }

    #[test]
    fn build_audit_event_carries_a_denied_response_code() {
        let event = build_audit_event("DELETE", "/api/v1/namespaces/default/pods/web-1", "", None, None, &test_peer(), 403, &BTreeMap::new());
        assert_eq!(event["responseStatus"]["code"], 403);
    }

    #[test]
    fn rejected_requests_are_written_to_the_audit_sink_without_a_policy() {
        let path = std::env::temp_dir().join(format!(
            "nodeapiserver-audit-rejected-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let sink = crate::audit::sink::AuditSink::open(&path).unwrap();
        let info = path::parse("GET", "/version", "");
        log_audit_rejected_request(
            "audit-id",
            &info,
            "GET",
            "/version",
            "",
            None,
            None,
            &test_peer(),
            401,
            Some(&sink),
            None,
        );

        let content = std::fs::read_to_string(&path).unwrap();
        let events: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["auditID"], "audit-id");
        assert_eq!(events[0]["stage"], "ResponseComplete");
        assert_eq!(events[0]["responseStatus"]["code"], 401);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn long_running_requests_are_not_logged_as_response_complete() {
        assert!(is_long_running_request(
            &path::parse("GET", "/api/v1/pods", "watch=true"),
            "watch=true"
        ));
        assert!(is_long_running_request(
            &path::parse("POST", "/api/v1/namespaces/default/pods/web/exec", ""),
            ""
        ));
        assert!(is_long_running_request(
            &path::parse(
                "GET",
                "/api/v1/namespaces/default/pods/web/log",
                "follow=true"
            ),
            "follow=true"
        ));
        assert!(!is_long_running_request(
            &path::parse(
                "GET",
                "/api/v1/namespaces/default/pods/web/log",
                "follow=false"
            ),
            "follow=false"
        ));
    }

    #[test]
    fn staged_audit_events_share_the_request_audit_id() {
        let audit_id = "11111111-1111-1111-1111-111111111111";
        let received = build_audit_event_at_stage(
            audit_id,
            crate::audit::event::STAGE_REQUEST_RECEIVED,
            "GET",
            "/api/v1/pods",
            "watch=true",
            None,
            None,
            &test_peer(),
            0,
            &BTreeMap::new(),
        );
        let started = build_audit_event_at_stage(
            audit_id,
            crate::audit::event::STAGE_RESPONSE_STARTED,
            "GET",
            "/api/v1/pods",
            "watch=true",
            None,
            None,
            &test_peer(),
            200,
            &BTreeMap::new(),
        );
        assert_eq!(received["auditID"], started["auditID"]);
        assert_eq!(received["stage"], "RequestReceived");
        assert_eq!(started["stage"], "ResponseStarted");
        assert_eq!(started["responseStatus"]["code"], 200);
    }

    #[test]
    fn admission_warnings_use_warning_code_299_and_are_header_safe() {
        let mut response = Response::new(body_from_bytes(Vec::new()));
        apply_admission_warnings(&mut response, &["policy \"failed\"\nnext".to_string()]);
        assert_eq!(response.headers().get("warning").unwrap(), "299 - \"policy \\\"failed\\\" next\"");
    }

    #[test]
    fn proxy_suffix_supports_the_normal_subresource_form() {
        let info = path::parse("GET", "/api/v1/namespaces/default/services/web:http/proxy/healthz", "");
        assert_eq!(info.resource, "services");
        assert_eq!(info.name, "web:http");
        assert_eq!(info.subresource, "proxy");
        assert_eq!(proxy_suffix(&info), "/healthz");
    }

    #[test]
    fn proxy_suffix_supports_the_legacy_proxy_prefix_form() {
        let info = path::parse("GET", "/api/v1/proxy/nodes/node-a/stats/summary", "");
        assert_eq!(info.verb, "proxy");
        assert_eq!(info.resource, "nodes");
        assert_eq!(info.name, "node-a");
        assert_eq!(proxy_suffix(&info), "/stats/summary");
    }
}
