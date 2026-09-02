
/// The "persist" half of [`patch`]: validates/defaults `candidate` (the
/// object [`patch_prepare`] produced, possibly further mutated by
/// admission in between) and writes it with the same real optimistic
/// concurrency [`update`] uses (`Txn`-compared-against-`ModRevision`,
/// via the shared [`persist_update`] tail) — no client-submitted
/// `resourceVersion` needed, since the object being patched *is* the one
/// [`patch_prepare`] already read. With `dry_run`, it performs all of the
/// same validation/defaulting and returns the candidate without writing.
pub async fn patch_persist(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, context: PatchContext, candidate: Value, dry_run: bool) -> Result<UpdateOutcome, Error> {
    patch_persist_with_manager(storage, group, version, resource, namespace, name, context, candidate, dry_run, None).await
}
