    let Value::Object(entries) = value else {
        return Err(type_mismatch(message, field, "object (map)", value));
    };
    let (key_type, value_type) = split_map_type(&field.proto_type)?;
    for (k, v) in entries {
        let mut entry = Vec::new();
        let key_field = ProtoField { message: field.message, json_name: "key", number: 1, repeated: false, map: false, proto_type: key_type };
        encode_scalar_or_message(message, &key_field, &Value::String(k.clone()), &mut entry)?;
        let value_field = ProtoField { message: field.message, json_name: "value", number: 2, repeated: false, map: false, proto_type: value_type };
        encode_scalar_or_message(message, &value_field, v, &mut entry)?;
        wire::encode_tag(field.number, WireType::LengthDelimited, out);
        wire::encode_length_delimited(&entry, out);
    }
