    let inner = json_schema_props_message_for(wrapper_message);
    let mut pos = 0;
    let mut schema_bytes: Option<&[u8]> = None;
    let mut list_items = Vec::new();
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        match field_number {
            1 => schema_bytes = Some(as_bytes("JSONSchemaPropsOrArray.schema", &raw)?),
            2 => list_items.push(decode_message(&inner, as_bytes("JSONSchemaPropsOrArray.jSONSchemas", &raw)?)?),
            _ => {}
        }
    }
    if !list_items.is_empty() {
        return Ok(Value::Array(list_items));
    }
