
async fn reconcile_crd_caches(
    storage: StorageClient,
    crd_cache: crate::cacher::SharedCache,
    registry: crate::cacher::CacheRegistry,
) {
    include!("body-11-1.rs");
    include!("body-11-2.rs");
}
