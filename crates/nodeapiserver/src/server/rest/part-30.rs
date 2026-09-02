
/// Applies a JSON/merge/strategic patch through the Pod
/// `ephemeralcontainers` subresource. The patch is evaluated against the
/// complete current Pod, then only its resulting ephemeral-container list
/// is retained, matching upstream's reset-fields strategy.
pub async fn patch_ephemeral_containers(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    kind_of_patch: PatchKind,
    patch_doc: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    include!("body-41-1.rs");
    include!("body-41-2.rs");
}
