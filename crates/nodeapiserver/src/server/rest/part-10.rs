
/// Validate and prune an object after a conversion webhook has produced the
/// representation that will be written. The request version's schema is not
/// sufficient here: a webhook may return an object that passed validation
/// before conversion but does not satisfy the storage version's schema.
fn revalidate_storage_object(schema: Option<&Value>, object: Value) -> Result<Value, Vec<String>> {
    include!("body-12-1.rs");
    include!("body-12-2.rs");
}
