
/// Resolve the served resource's namespacedness for admission matching. The
/// static discovery table handles built-ins without I/O; a CRD lookup uses
/// the same established definitions as ordinary REST resolution.
pub async fn resource_is_namespaced(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<bool>, Error> {
    include!("body-5-1.rs");
    include!("body-5-2.rs");
}
