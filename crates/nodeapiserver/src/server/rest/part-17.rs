
/// [`get`] with an optional etcd MVCC snapshot revision. A non-positive
/// revision retains the normal current-state behavior.
pub async fn get_at_revision(storage: &mut StorageClient, cache: Option<&crate::cacher::store::SharedCache>, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, resource_version: i64) -> Result<GetOutcome, Error> {
    include!("body-23-1.rs");
    include!("body-23-2.rs");
}
