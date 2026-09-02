
/// Lists the objects selected by a collection delete without changing them.
/// The listener uses this first so it can run admission against each matched
/// object before calling [`delete`].
pub async fn list_delete_collection(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
) -> Result<DeleteCollectionOutcome, Error> {
    include!("body-86-1.rs");
    include!("body-86-2.rs");
}
