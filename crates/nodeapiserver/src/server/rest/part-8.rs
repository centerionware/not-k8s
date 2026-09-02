
async fn convert_to_storage_version(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    conversion_webhook: Option<&apiextensions::registry::ConversionWebhook>,
    object: Value,
) -> Result<Value, Error> {
    include!("body-10-1.rs");
    include!("body-10-2.rs");
}
