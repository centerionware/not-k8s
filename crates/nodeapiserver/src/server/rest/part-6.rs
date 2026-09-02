
/// Resolve the OpenAPI schema used to declare CEL mutation object aliases.
/// Built-in schemas come from the same vendored document advertised by
/// `/openapi/v3`; CRD schemas come from their established version directly.
/// Built-in references are expanded here so the CEL environment can register
/// names such as `Object.spec.containers` without duplicating schema lookup
/// rules in the admission layer.
pub async fn mutation_openapi_schema(storage: &mut StorageClient, group: &str, version: &str, resource: &str) -> Result<Option<Value>, Error> {
    include!("body-8-1.rs");
    include!("body-8-2.rs");
}
