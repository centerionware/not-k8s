    let inner = json_schema_props_message_for(wrapper_message);
    let mut out = Vec::new();
    match value {
        Value::Array(items) => {
            for item in items {
                let bytes = encode_message(&inner, item)?;
                wire::encode_tag(2, WireType::LengthDelimited, &mut out);
                wire::encode_length_delimited(&bytes, &mut out);
            }
        }
        _ => {
            let bytes = encode_message(&inner, value)?;
            wire::encode_tag(1, WireType::LengthDelimited, &mut out);
            wire::encode_length_delimited(&bytes, &mut out);
        }
    }
