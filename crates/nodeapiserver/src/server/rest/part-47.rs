
/// Server-Side Apply (`PATCH` with `Content-Type: application/apply-
/// patch+yaml`) — real upstream's `merge.Updater.Apply`, wired to real
/// storage (`crate::patch::updater::apply`,
/// `crate::patch::managed_fields`). `config` is the apply configuration,
/// already decoded from the request body by the caller (YAML or JSON —
/// real upstream accepts either for this content type, and this crate's
/// existing content negotiation already handles both for every other
/// verb).
///
/// Handles both real cases: an already-existing object (reads its
/// stored `managedFields`, runs `updater::apply` against it, persists
/// with the same optimistic-concurrency `Txn` every other write verb
/// uses) and **create-on-apply** (no object exists at this key yet —
/// real upstream's own Apply can create one, `liveObject` starting
/// empty; this branch runs the identical `updater::apply` orchestration
/// against an empty `live`, then persists with the same
/// create-only-if-absent `Txn` idiom `create`'s own doc comment names,
/// rather than `persist_update`'s update-if-matches one).
///
/// Named `server_side_apply`, not `apply_patch` — that name is already
/// this module's own private helper for the three ordinary patch kinds
/// (`json_patch`/`merge_patch`/`strategic_merge`) just above; this is a
/// wholly different real orchestration, not a fourth branch of that one.
///
/// A convenience wrapper combining [`apply_prepare`] and
/// [`apply_persist`] with no admission step in between — the same shape
/// [`patch`] is to [`patch_prepare`]/[`patch_persist`]. `server::
/// listener`'s own real request handler calls the two halves directly
/// instead, so it can run Group J's `LimitRanger` PVC check against the
/// real candidate object in between, the same way it already does for
/// the three-patch-kind `PATCH` path.
///
/// A CRD-defined resource with an established structural schema uses the
/// runtime-schema SSA path; malformed or schema-less CRD records retain the
/// defensive [`ApplyOutcome::UnsupportedForCrd`] outcome.
pub async fn server_side_apply(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, manager: &str, force: bool, config: &Value) -> Result<ApplyOutcome, Error> {
    match apply_prepare(storage, group, version, resource, namespace, name, manager, force, config).await? {
        ApplyPrepareOutcome::Ready(candidate, context) => apply_persist(storage, group, version, resource, namespace, context, candidate, false).await,
        ApplyPrepareOutcome::UnknownResource => Ok(ApplyOutcome::UnknownResource),
        ApplyPrepareOutcome::Conflict(c) => Ok(ApplyOutcome::Conflict(c)),
        ApplyPrepareOutcome::Invalid(v) => Ok(ApplyOutcome::Invalid(v)),
        ApplyPrepareOutcome::NoOp(v) => Ok(ApplyOutcome::NoOp(v)),
        ApplyPrepareOutcome::UnsupportedForCrd => Ok(ApplyOutcome::UnsupportedForCrd),
    }
}

/// The context [`apply_prepare`] hands back to [`apply_persist`] once the
/// merged, pruned, conflict-checked, validated, defaulted candidate is
/// ready — enough for a caller (`server::listener`) to run Group J
/// admission (`LimitRanger`'s own PVC check) against the real candidate
/// in between, the same split [`PatchContext`] already exists for.
#[derive(Debug)]
pub struct ApplyContext {
    /// `Some` for a built-in compiled schema and `None` for a CRD whose
    /// runtime schema has already been consumed during preparation.
    schema: Option<&'static str>,
    storage_open_api_schema: Option<Value>,
    kind: String,
    conversion_webhook: Option<apiextensions::registry::ConversionWebhook>,
    key: String,
    /// `Some((existing_kv, live))` for an update-on-apply (persisted via
    /// [`persist_update`]'s update-if-matches `Txn`); `None` for
    /// create-on-apply (persisted via the same create-only-if-absent
    /// `Txn` idiom [`create`]'s own doc comment names).
    existing: Option<(mvccpb::KeyValue, Value)>,
}
