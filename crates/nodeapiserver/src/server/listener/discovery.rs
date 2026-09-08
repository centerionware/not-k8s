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
    FoundOpenApiProtobuf(&'static [u8]),
    NotAcceptable,
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
    let Some(header) = accept_header else {
        return false;
    };
    let Some(accepted) = negotiation::negotiate(header) else {
        return false;
    };
    accepted.as_kind.as_deref() == Some("APIGroupDiscoveryList")
        && accepted.as_group.as_deref() == Some("apidiscovery.k8s.io")
        && accepted.as_version.as_deref() == Some("v2")
}

const AGGREGATED_DISCOVERY_CONTENT_TYPE: &str =
    "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList";

fn discovery_content_type(parts: &[String], accept_header: Option<&str>) -> &'static str {
    if parts.len() == 1
        && matches!(
            parts.first().map(String::as_str),
            Some("api") | Some("apis")
        )
        && wants_aggregated_discovery(accept_header)
    {
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
fn aggregated_discovery_group_version<'a>(
    parts: &'a [String],
    aggregated: &'a [(String, String)],
) -> Option<(&'a str, &'a str)> {
    if parts.len() != 3 || parts[0] != "apis" {
        return None;
    }
    aggregated
        .iter()
        .find(|(g, v)| g == &parts[1] && v == &parts[2])
        .map(|(g, v)| (g.as_str(), v.as_str()))
}

fn route_discovery(
    parts: &[String],
    accept_header: Option<&str>,
    crds: &[crate::apiextensions::registry::DiscoverableResource],
    aggregated: &[(String, String)],
) -> DiscoveryRoute {
    let seg = |i: usize| parts.get(i).map(String::as_str);
    match (seg(0), seg(1), parts.len()) {
        (Some("api"), _, 1) if wants_aggregated_discovery(accept_header) => {
            DiscoveryRoute::Found(discovery::api_v1_group_discovery_list_with_crds())
        }
        (Some("api"), _, 1) => DiscoveryRoute::Found(discovery::api_versions()),
        (Some("api"), _, 2) => match discovery::api_resource_list("", &parts[1]) {
            Some(doc) => DiscoveryRoute::Found(doc),
            None => DiscoveryRoute::NotFound,
        },
        (Some("apis"), _, 1) if wants_aggregated_discovery(accept_header) => DiscoveryRoute::Found(
            discovery::api_group_discovery_list_with_crds(crds, aggregated),
        ),
        (Some("apis"), _, 1) => {
            DiscoveryRoute::Found(discovery::api_group_list_with_crds(crds, aggregated))
        }
        (Some("apis"), _, 2) => match discovery::api_group_with_crds(&parts[1], crds, aggregated) {
            Some(doc) => DiscoveryRoute::Found(doc),
            None => DiscoveryRoute::NotFound,
        },
        (Some("apis"), _, 3) => {
            match discovery::api_resource_list_with_crds(&parts[1], &parts[2], crds) {
                Some(doc) => DiscoveryRoute::Found(doc),
                None => DiscoveryRoute::NotFound,
            }
        }
        (Some("openapi"), Some("v2"), 2) => match openapi::negotiate_v2(accept_header) {
            Some(true) => DiscoveryRoute::FoundOpenApiProtobuf(openapi::v2_protobuf()),
            Some(false) => DiscoveryRoute::Found(openapi::v2()),
            None => DiscoveryRoute::NotAcceptable,
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

fn forbidden_status_with_reason(
    path_str: &str,
    user_name: &str,
    reason: &str,
) -> serde_json::Value {
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
        admission::webhook::Error::DryRunUnsupported { detail, .. } => json_response(
            StatusCode::BAD_REQUEST,
            &bad_request_status(path_str, detail),
        ),
        _ => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &internal_error_status(path_str),
        ),
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

/// Real upstream's own `Conflict` shape for an `UPDATE`/`PATCH` (including
/// the scale/status/pod-resize/other subresource writes that reuse the
/// same `persist_update` tail) that lost the optimistic-concurrency
/// `ModRevision` compare — `reason: "Conflict"`, `code: 409`
/// (`apierrors.NewConflict`). This is a **different** real status from
/// [`conflict_status`]'s `AlreadyExists`: upstream reserves that reason
/// for a `CREATE` that lost the create-only-if-absent race specifically,
/// and real client-go code branches on the two differently
/// (`apierrors.IsConflict()` is what a controller's own read-modify-write
/// retry loop checks — it does not match `AlreadyExists`). Every
/// `UpdateOutcome::Conflict`/`ScaleOutcome::Conflict` call site should use
/// this, not [`conflict_status`], even though both currently produce
/// HTTP 409.
fn update_conflict_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: the object has been modified; please apply your changes to the latest version and try again"),
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
                .map_or(("", violation.as_str()), |(field, message)| {
                    (field, message)
                });
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
async fn persist_quota_usage_updates(
    client: &mut StorageClient,
    namespace: &str,
    updates: Vec<(
        String,
        std::collections::BTreeMap<String, crate::scheme::quantity::Quantity>,
    )>,
    path_str: &str,
) {
    for (quota_name, new_usage) in updates {
        for _attempt in 0..3 {
            let current = match rest::get(
                client,
                None,
                "",
                "v1",
                "resourcequotas",
                Some(namespace),
                &quota_name,
            )
            .await
            {
                Ok(rest::GetOutcome::Found(q)) => q,
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => {
                    break;
                }
                Err(e) => {
                    warn!(path = %path_str, %quota_name, error = ?e, "admission: reading ResourceQuota to persist status.used failed");
                    break;
                }
            };
            let mut merged: std::collections::BTreeMap<String, crate::scheme::quantity::Quantity> =
                current
                    .pointer("/status/used")
                    .and_then(serde_json::Value::as_object)
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| {
                                v.as_str()
                                    .and_then(|s| crate::scheme::quantity::Quantity::parse(s).ok())
                                    .map(|q| (k.clone(), q))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            for (k, v) in &new_usage {
                merged.insert(k.clone(), *v);
            }
            let mut status_body = current.clone();
            status_body["status"]["used"] = serde_json::Value::Object(
                merged
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.to_string())))
                    .collect(),
            );

            match rest::update_status(
                client,
                "",
                "v1",
                "resourcequotas",
                Some(namespace),
                &quota_name,
                &status_body,
                false,
            )
            .await
            {
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
