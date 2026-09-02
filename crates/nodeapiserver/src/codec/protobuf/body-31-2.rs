    match schema_bytes {
        Some(b) => decode_message(&inner, b),
        None => Ok(Value::Bool(allows)),
    }
