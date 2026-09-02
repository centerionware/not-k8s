
/// Replaces a Pod through the `ephemeralcontainers` subresource. Only the
/// ephemeral-container list from `body` is retained; spec, status, and
/// ordinary metadata changes are discarded by the subresource strategy.
pub async fn update_ephemeral_containers(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    include!("body-40-1.rs");
    include!("body-40-2.rs");
}
