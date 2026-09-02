
fn reconcile_crd_cache(
    storage: &StorageClient,
    registry: &crate::cacher::CacheRegistry,
    state: &mut DynamicCacheState,
    crd_key: Vec<u8>,
    crd: Option<&serde_json::Value>,
) {
    include!("body-10-1.rs");
    include!("body-10-2.rs");
}
