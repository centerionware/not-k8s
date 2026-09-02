
/// Real upstream's generic status subresource (`GenericStatusREST`,
/// `k8s.io/apiserver/pkg/registry/generic/registry/store.go`'s own
/// `StatusREST`): a `PUT` through `<resource>/status` only ever changes
/// the object's `status` field — every other top-level field on the
/// submitted body (`spec`, most of `metadata`) is ignored, the existing
/// object's own spec/metadata survives untouched apart from the same
/// `creationTimestamp`/`uid` immutability [`persist_update`] already
/// enforces for a plain `update`. Same real optimistic concurrency as
/// `update` (submitted `metadata.resourceVersion` must match).
///
/// For a CRD-defined resource, the matched version's `status` schema is
/// applied to the replacement status: unknown fields are pruned and the
/// schema's required/type/local constraints are validated, just as for the
/// main resource. Built-in status strategies remain the generic, untyped
/// path because their per-kind status rules are hand-written upstream and
/// are not represented by this crate's generic discovery table. The
/// namespace-mismatch check `update` runs against the body is skipped (moot
/// here — the body's own `metadata`/`spec` are never read for anything but
/// `resourceVersion`). [`patch_status`] is this function's `PATCH` counterpart.
///
/// A CRD-defined resource whose matched version never declared
/// `subresources.status` has no `status` subresource at all — real
/// upstream doesn't even install this route for such a version — so
/// this returns `UnknownResource` (a real `404`) rather than silently
/// serving a status write real upstream itself would refuse. Every
/// built-in resource this crate resolves through the static table is
/// unaffected: `resolve_resource` always reports `true` for one, the
/// same "not modeled per-type yet" scope this crate's own discovery
/// already has for built-in subresources generally.
/// `dry_run` validates and returns the status candidate without persisting it.
pub async fn update_status(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, body: &Value, dry_run: bool) -> Result<UpdateOutcome, Error> {
    update_status_with_manager(storage, group, version, resource, namespace, name, body, dry_run, None).await
}
