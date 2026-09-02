
fn preserve_managed_fields(existing: &Value, object: &mut Value) {
    if let Some(fields) = existing.pointer("/metadata/managedFields").cloned() {
        set_metadata_field(object, "managedFields", fields);
    } else if let Some(metadata) = object.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove("managedFields");
    }
}
