
/// Replaces an existing object. `namespace: None` for a cluster-scoped
/// resource, same convention as [`get`]/[`create`]. Real optimistic
/// concurrency: reads the current object first, requires the submitted
/// body's own `metadata.resourceVersion` to match what's actually
/// stored, and writes with a `Txn` compared against that same revision
/// — a concurrent write between the read and this write loses the race
/// and gets a real `Conflict`, not a silent overwrite.
/// `metadata.creationTimestamp`/`uid` are preserved from the existing
/// object regardless of what the client submitted — real upstream
/// treats both as immutable after creation.
pub async fn update(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, body: &Value) -> Result<UpdateOutcome, Error> {
    update_with_options_and_manager(storage, group, version, resource, namespace, name, body, false, None).await
}

/// [`update`] with the real Kubernetes `dryRun=All` write option. The
/// candidate is prepared exactly like a normal update, but the final
/// optimistic-concurrency write is omitted.
pub async fn update_with_options(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, body: &Value, dry_run: bool) -> Result<UpdateOutcome, Error> {
    update_with_options_and_manager(storage, group, version, resource, namespace, name, body, dry_run, None).await
}
