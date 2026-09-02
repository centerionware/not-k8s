
/// The dynamic (CRD-only) half of [`resolve_resource`] — skips the
/// static `resolve_kind` check entirely, so it's only ever correct to
/// call once a caller has already ruled that out itself.
/// `server::listener`'s own `WATCH` dispatch is the other real caller
/// besides [`resolve_resource`]: `watch` is served straight from an
/// already-registered `cacher::store::SharedCache` rather than through
/// any of this module's own generic verb functions, so it has no other
/// reason to reach into `server::rest` for a CRD-defined resource at
/// all — it needs only the Kind a matching `Established` CRD resolves
/// to, both to spawn a cache for it on first watch
/// (`cacher::registry::CacheRegistry::spawn`, callable at any time, not
/// just at boot) and to label the watch events it then streams.
async fn resolve_crd(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<apiextensions::registry::CrdResource>, Error> {
    include!("body-13-1.rs");
    include!("body-13-2.rs");
}
