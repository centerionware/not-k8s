    if field.map {
        return encode_map_field(message, field, value, out);
    }
    if field.repeated {
        let Value::Array(items) = value else {
            return Err(type_mismatch(message, field, "array", value));
        };
        for item in items {
            encode_scalar_or_message(message, field, item, out)?;
        }
        return Ok(());
    }
