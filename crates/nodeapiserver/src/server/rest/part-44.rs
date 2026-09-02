
/// [`patch_status`] with the request's field manager.
pub async fn patch_status_with_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    kind_of_patch: PatchKind,
    patch_doc: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    include!("body-61-1.rs");
    include!("body-61-2.rs");
}
