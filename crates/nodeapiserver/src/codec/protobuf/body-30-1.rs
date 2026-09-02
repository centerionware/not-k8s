    let mut out = Vec::new();
    match value {
        Value::Bool(allows) => {
            // `false` is proto2's own zero value for an optional bool --
            // omitted entirely, matching every other scalar field's own
            // "absent means default" convention this codec already
            // established elsewhere (`encode_time_string`'s own doc
            // comment on the epoch case is the same idiom).
            if *allows {
                wire::encode_tag(1, WireType::Varint, &mut out);
                wire::encode_varint(1, &mut out);
            }
        }
        _ => {
            let inner = json_schema_props_message_for(wrapper_message);
            let bytes = encode_message(&inner, value)?;
            wire::encode_tag(2, WireType::LengthDelimited, &mut out);
            wire::encode_length_delimited(&bytes, &mut out);
        }
    }
