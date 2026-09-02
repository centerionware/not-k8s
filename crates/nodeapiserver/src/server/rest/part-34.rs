
/// [`update_status`] with the request's field manager. Status writes use a
/// separate managed-fields subresource entry, as in upstream.
pub async fn update_status_with_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    include!("body-46-1.rs");
    include!("body-46-2.rs");
}
