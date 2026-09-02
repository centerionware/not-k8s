
/// [`list`] with an optional etcd MVCC snapshot revision. A positive
/// revision bypasses the live watch cache and returns a consistent snapshot
/// from nodestore.
pub async fn list_at_revision(
    storage: &mut StorageClient,
    cache: Option<&crate::cacher::store::SharedCache>,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
    limit: i64,
    continue_token: &str,
    resource_version: i64,
) -> Result<ListOutcome, Error> {
    include!("body-27-1.rs");
    include!("body-27-2.rs");
}
