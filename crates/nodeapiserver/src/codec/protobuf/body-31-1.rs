    let inner = json_schema_props_message_for(wrapper_message);
    let mut pos = 0;
    let mut allows = false;
    let mut schema_bytes: Option<&[u8]> = None;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        match field_number {
            1 => allows = as_varint("JSONSchemaPropsOrBool.allows", &raw)? != 0,
            2 => schema_bytes = Some(as_bytes("JSONSchemaPropsOrBool.schema", &raw)?),
            _ => {}
        }
    }
