
/// Convenience wrapper combining [`patch_prepare`] and [`patch_persist`]
/// with no admission step in between — what `server::rest::patch` used
/// to do as one function before the split; kept for any caller that
/// doesn't need to run admission in the middle (this crate's own tests).
pub async fn patch(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, kind_of_patch: PatchKind, patch_doc: &Value) -> Result<UpdateOutcome, Error> {
    match patch_prepare(storage, group, version, resource, namespace, name, kind_of_patch, patch_doc).await? {
        PatchPrepareOutcome::Ready(candidate, context) => patch_persist(storage, group, version, resource, namespace, name, context, candidate, false).await,
        PatchPrepareOutcome::UnknownResource => Ok(UpdateOutcome::UnknownResource),
        PatchPrepareOutcome::ObjectNotFound => Ok(UpdateOutcome::ObjectNotFound),
        PatchPrepareOutcome::Invalid(v) => Ok(UpdateOutcome::Invalid(v)),
    }
}
