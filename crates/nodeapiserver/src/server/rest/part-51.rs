
/// The "persist" half of [`server_side_apply`]: writes `object` (the
/// candidate [`apply_prepare`] produced, possibly further mutated by
/// admission in between) with whichever real `Txn` idiom
/// [`ApplyContext::existing`] calls for.
pub async fn apply_persist(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, context: ApplyContext, mut object: Value, dry_run: bool) -> Result<ApplyOutcome, Error> {
    include!("body-69-1.rs");
    include!("body-69-2.rs");
}
