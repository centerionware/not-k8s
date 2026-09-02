
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BoxedBody = http_body_util::combinators::BoxBody<hyper::body::Bytes, BoxError>;

type DynamicCacheState = HashMap<Vec<u8>, HashSet<crate::cacher::registry::ResourceKey>>;

#[derive(Debug, Clone, Default)]
struct AdmissionMetadata {
    warnings: Vec<String>,
    audit_failures: Vec<Value>,
}

type SharedAdmissionMetadata = Arc<Mutex<AdmissionMetadata>>;
