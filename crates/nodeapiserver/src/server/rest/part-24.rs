
/// [`create_with_options`] with the request's field manager. The listener
/// supplies the explicit `fieldManager` or the request's user agent, just as
/// upstream's `managerOrUserAgent` does. Direct REST callers may omit it;
/// their submitted `managedFields` are never trusted or persisted.
pub async fn create_with_options_and_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<CreateOutcome, Error> {
    include!("body-33-1.rs");
    include!("body-33-2.rs");
}
