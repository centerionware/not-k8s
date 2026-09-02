    match schema_bytes {
        Some(b) => decode_message(&inner, b),
        // Real zero value: no `items` schema was ever actually written
        // (both fields absent) -- an empty schema, matching what a
        // never-set `*JSONSchemaProps` field marshals to.
        None => Ok(Value::Object(Map::new())),
    }
