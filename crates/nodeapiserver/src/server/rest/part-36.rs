
/// The tail [`update`] and [`patch`] share once each has its own
/// candidate object in hand (a defaulted submitted body for `update`, a
/// patch-applied one for `patch`): preserve `creationTimestamp`/`uid`
/// from the existing object (real upstream treats both as immutable
/// after creation, regardless of what the caller's patch/body touched),
/// stamp the namespace, then a real optimistic-concurrency `Txn`
/// compared against the exact revision both callers already read —
/// a concurrent write between that read and this write loses the race
/// and gets a real `Conflict`, not a silent overwrite.
async fn persist_update(
    storage: &mut StorageClient,
    schema: Option<&str>,
    open_api_schema: Option<&Value>,
    storage_open_api_schema: Option<&Value>,
    kind: &str,
    group: &str,
    version: &str,
    resource: &str,
    key: String,
    existing_kv: &mvccpb::KeyValue,
    existing_object: &Value,
    namespace: Option<&str>,
    mut object: Value,
    dry_run: bool,
    conversion_webhook: Option<apiextensions::registry::ConversionWebhook>,
    field_manager: Option<&str>,
    managed_subresource: &str,
    managed_fields_reconciled: bool,
) -> Result<UpdateOutcome, Error> {
    include!("body-48-1.rs");
    include!("body-48-2.rs");
}
