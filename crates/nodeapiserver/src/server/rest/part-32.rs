
/// [`update_with_options`] with the request's field manager. Ordinary
/// updates use the same `Update` managed-fields operation as upstream and do
/// not report ownership conflicts; changed fields move to this manager.
pub async fn update_with_options_and_manager(
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
    include!("body-44-1.rs");
    include!("body-44-2.rs");
}
