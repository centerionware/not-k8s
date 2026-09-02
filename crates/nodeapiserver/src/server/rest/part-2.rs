
/// Resolves a parameter kind from a `ValidatingAdmissionPolicy`'s
/// `spec.paramKind`. Parameter kinds carry an API group and Kind but no
/// version or resource plural, so choose the most-preferred served version
/// from the static discovery table, then fall back to an Established CRD.
/// This is intentionally a read-only inverse of the normal resource lookup;
/// callers still use [`get`]` and [`list`]` for the actual parameter object.
pub async fn resolve_resource_for_kind(storage: &mut StorageClient, group: &str, kind: &str) -> Result<Option<(String, String, String, bool)>, Error> {
    include!("body-4-1.rs");
    include!("body-4-2.rs");
}
