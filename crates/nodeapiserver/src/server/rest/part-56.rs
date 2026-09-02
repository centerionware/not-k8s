
fn reconcile_managed_fields(
    schema: Option<&str>,
    open_api_schema: Option<&Value>,
    existing: &Value,
    mut object: Value,
    field_manager: Option<&str>,
    operation: &str,
    subresource: &str,
    group: &str,
    version: &str,
) -> Value {
    include!("body-74-1.rs");
    include!("body-74-2.rs");
}
