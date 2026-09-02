
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
