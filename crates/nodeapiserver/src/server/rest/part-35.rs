
/// Applies a CRD version's `properties.status` schema to a status-subresource
/// candidate. Wrapping the status value in a synthetic top-level object lets
/// the existing schema walkers report the correct `status.foo` paths and
/// check the status root's own type without duplicating their logic. The
/// returned value is already pruned; it is only written when validation
/// succeeds.
fn validate_crd_status(open_api_schema: &Option<Value>, object: &mut Value) -> Vec<String> {
    include!("body-47-1.rs");
    include!("body-47-2.rs");
}
