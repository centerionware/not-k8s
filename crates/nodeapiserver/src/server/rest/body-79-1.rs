    let Some(map) = object.as_object_mut() else { return };
    map.insert("kind".to_string(), Value::String(kind.to_string()));
