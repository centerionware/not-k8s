
#[derive(Debug, PartialEq)]
pub enum ListOutcome {
    /// The real `<Kind>List` document, ready to serialize.
    Found(Value),
    UnknownResource,
    /// The submitted `continue` token didn't decode — not valid base64,
    /// no `0x00` key/revision separator, or a non-numeric revision.
    /// Real upstream's own `errors.NewBadRequest("continue token is not
    /// valid")` shape, not a `500`.
    InvalidContinueToken,
}

/// The real `<Kind>List` `kind` value for a resource this build serves —
/// standard Kubernetes convention, verified against real vendored data:
/// every List type in the vendored OpenAPI specs is named exactly
/// `<Kind>List` (`PodList`, `DeploymentList`, ...), never a separate
/// hand-assigned name.
fn list_kind(kind: &str) -> String {
    format!("{kind}List")
}

/// Lists every object of a resource — the whole resource, or scoped to
/// one namespace (`namespace: None` for a cluster-scoped resource, same
/// convention as [`get`]). Items are decoded and filtered
/// (`cacher::selector::object_matches`) the same way regardless of source.
/// Items are returned in whatever order the source hands them back in
/// (key order, for both a real `Range` and the cache's own `BTreeMap`) —
/// real upstream doesn't guarantee list ordering either.
/// `label_selector`/`field_selector` are the raw query-string values
/// `path::RequestInfo` already captures for `list` (empty means "no
/// constraint from that half," matching upstream's own `Everything()`
/// selector semantics). `limit`/`continue_token` are real pagination —
/// `limit <= 0` means "no limit" (matching real upstream's own `0`
/// convention), and a non-empty `continue_token` resumes an earlier
/// paginated listing (real upstream's own contract: opaque to the
/// client, only ever handed back verbatim from a prior page's own
/// `metadata.continue`). A paginated request always bypasses the watch
/// cache (see below) and reads directly from nodestore, since real
/// pagination is a genuine ordered range-scan-with-resume-point, which
/// the cache's own unordered in-memory store doesn't support. Real
/// upstream's own documented caveat applies here too: label/field
/// selector filtering happens *after* the limited range fetch, so a
/// page can come back with fewer than `limit` items (even zero) despite
/// more matching items existing on later pages.
///
/// `cache`, if given, is consulted first — but only once
/// [`crate::cacher::store::SharedCache::has_synced`] is true. Unlike
/// [`get`]'s "a miss always falls through" trick, `list` can't use that
/// same safety net: a cache that hasn't finished its first `LIST` yet
/// would report zero items, and zero items is itself a fully valid `LIST`
/// answer (a real `200`, not a `404`) — there is no way to tell "empty
/// because unsynced" from "empty because genuinely empty" after the fact,
/// so this checks `has_synced()` up front instead (see that method's own
/// doc comment for why it's a real flag, not inferred from the revision).
/// An unsynced cache falls through to nodestore exactly as `cache: None`
/// would. `None` behaves exactly as before this parameter existed; callers
/// outside the listener's cache path still pass `None` (same scope as
/// `get`'s own cache parameter).
pub async fn list(
    storage: &mut StorageClient,
    cache: Option<&crate::cacher::store::SharedCache>,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
    limit: i64,
    continue_token: &str,
) -> Result<ListOutcome, Error> {
    list_at_revision(storage, cache, group, version, resource, namespace, label_selector, field_selector, limit, continue_token, 0).await
}
