
/// [`patch_persist`] with the request's field manager. Ordinary patch writes
/// use the same managed-fields `Update` operation as ordinary PUT writes.
pub async fn patch_persist_with_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    context: PatchContext,
    candidate: Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    include!("body-59-1.rs");
    include!("body-59-2.rs");
}
