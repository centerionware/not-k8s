    if let Some(metadata) = object.get_mut("metadata").and_then(Value::as_object_mut) {
        for field in ["resourceVersion", "creationTimestamp", "selfLink", "uid", "managedFields"] {
            metadata.remove(field);
        }
    }
