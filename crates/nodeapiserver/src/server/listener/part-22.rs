
type WatchEventStream = Pin<Box<dyn tokio_stream::Stream<Item = crate::cacher::store::WatchEvent> + Send + Sync>>;
type WatchFrameFuture = Pin<Box<dyn Future<Output = Option<Result<hyper::body::Frame<hyper::body::Bytes>, BoxError>>> + Send>>;

struct ConversionWatchState {
    events: WatchEventStream,
    pending: Option<WatchFrameFuture>,
    kind: String,
    api_version: String,
    storage: Option<StorageClient>,
    group: String,
    resource: String,
    version: String,
    partial_metadata: bool,
    conversion_webhook: Option<crate::apiextensions::registry::ConversionWebhook>,
}

struct ConversionWatchStream {
    state: Arc<Mutex<ConversionWatchState>>,
}
