
fn has_finalizers(object: &Value) -> bool {
    object
        .pointer("/metadata/finalizers")
        .and_then(Value::as_array)
        .is_some_and(|finalizers| !finalizers.is_empty())
}

fn has_deletion_timestamp(object: &Value) -> bool {
    object
        .pointer("/metadata/deletionTimestamp")
        .is_some_and(|timestamp| !timestamp.is_null())
}
