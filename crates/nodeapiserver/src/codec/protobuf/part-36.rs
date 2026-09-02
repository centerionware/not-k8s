
/// Which schema `resolve_message_ref` and the type-meta bridge both need:
/// looks up `(group, version, kind)` the way [`unwrap_unknown`]'s caller
/// would after splitting `apiVersion` into group/version.
pub fn schema_for_gvk(group: &str, version: &str, kind: &str) -> Option<&'static str> {
    codegen::schema_by_gvk().get(&(group, version, kind)).copied()
}
