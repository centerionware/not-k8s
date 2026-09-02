
/// The single place every real verb in this module decides what
/// `(group, version, resource)` actually is: the static, build-time
/// table first (no I/O, the overwhelmingly common case), falling back to
/// a live `LIST` of `CustomResourceDefinition`s only on a miss — Group
/// K's dynamic resource registry. `None` either way means a genuine
/// `UnknownResource` outcome to the caller, exactly as `resolve_kind`
/// alone used to mean.
///
/// **The CRD group itself is never recursed into** (`group.is_empty()`
/// covers the core group, which by definition has no CRDs in it
/// either): a request for `apiextensions.k8s.io/v1/customresourcedefinitions`
/// is always answered by the static table (Group A's codegen already
/// covers it — a `CustomResourceDefinition` is a real, compiled built-in
/// type, only the resources *it defines* are dynamic), so there's no risk
/// of this function ever listing CRDs to resolve a request for CRDs.
async fn resolve_resource(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<ResolvedResource>, Error> {
    include!("body-7-1.rs");
    include!("body-7-2.rs");
}
