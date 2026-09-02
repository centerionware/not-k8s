
async fn encode_watch_event_with_conversion(
    event: &crate::cacher::store::WatchEvent,
    kind: &str,
    api_version: &str,
    storage: Option<StorageClient>,
    group: &str,
    resource: &str,
    version: &str,
    partial_metadata: bool,
    conversion_webhook: Option<crate::apiextensions::registry::ConversionWebhook>,
) -> Option<Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>> {
    include!("body-30-1.rs");
    include!("body-30-2.rs");
}
