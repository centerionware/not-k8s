
async fn convert_to_requested_version(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    kind: &str,
    conversion_webhook: Option<&apiextensions::registry::ConversionWebhook>,
    object: Value,
) -> Result<Value, Error> {
    include!("body-11-1.rs");
    include!("body-11-2.rs");
}
