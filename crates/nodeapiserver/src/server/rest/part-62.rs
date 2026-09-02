
/// Deletes a single object with optional `DeleteOptions` preconditions and
/// `dryRun=All`. The read and delete/termination marker are joined by an
/// MVCC compare so a concurrent update cannot make a validated delete remove
/// or mark a newer object.
pub async fn delete_with_options(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    preconditions: Option<&DeletePreconditions>,
    dry_run: bool,
) -> Result<DeleteOutcome, Error> {
    include!("body-84-1.rs");
    include!("body-84-2.rs");
}
