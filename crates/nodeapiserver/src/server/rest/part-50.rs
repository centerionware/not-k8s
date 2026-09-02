
fn prune_runtime_schema(schema: Option<&Value>, value: Value) -> Value {
    match schema {
        Some(schema) => apiextensions::schema_pruning::prune(schema, &value),
        None => value,
    }
}
