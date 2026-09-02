
/// Apply a CEL `MutatingAdmissionPolicy` apply configuration to an admission
/// object. Apply configurations use the same strategic-merge rules as the
/// server's strategic-merge PATCH path; built-ins use their generated schema
/// and CRDs use their runtime OpenAPI schema. A resource without either
/// schema falls back to JSON merge semantics, which preserves the generic
/// server's behavior for schema-less resources.
pub async fn apply_admission_configuration(storage: &mut StorageClient, group: &str, version: &str, resource: &str, existing: &Value, configuration: &Value) -> Result<Value, Error> {
    include!("body-53-1.rs");
    include!("body-53-2.rs");
}
