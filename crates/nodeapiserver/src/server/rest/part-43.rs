
/// `PATCH .../status` — the patch counterpart to [`update_status`],
/// closing the "PUT-only" gap `docs/APISERVER.md` named for it. Applies
/// the patch to the whole existing object (same
/// [`apply_patch`] `patch_prepare` uses — real upstream's own subresource
/// PATCH semantics let the patch document reference any path), then
/// takes only the result's own `.status` field and merges it onto the
/// existing object exactly the way `update_status` does, so a
/// `strategic-merge-patch+json` `{"status": {...}}` document behaves the
/// same whether it arrives via `PUT` (full replace) or `PATCH` (merged).
/// No client-submitted `resourceVersion` needed, same as `patch_persist`.
/// The CRD status schema is applied to the patched status with the same
/// pruning and local validation as [`update_status`]. Built-in status
/// strategies remain the generic, untyped path. There is still no Group J
/// admission here — and the same
/// `subresources.status`-must-be-declared gate for a CRD-defined
/// resource (`update_status`'s own doc comment covers why). `dry_run` keeps
/// the same validation path while skipping the write.
pub async fn patch_status(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value, dry_run: bool) -> Result<UpdateOutcome, Error> {
    patch_status_with_manager(storage, group, version, resource, namespace, name, kind_of_patch, patch_doc, dry_run, None).await
}
