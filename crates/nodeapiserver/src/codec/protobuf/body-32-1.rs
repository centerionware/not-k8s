    let (key_type, value_type) = split_map_type(&field.proto_type)?;
    let entry_bytes = as_bytes(&format!("{message}.{}", field.json_name), raw)?;
    let key_field = ProtoField { message: field.message, json_name: "key", number: 1, repeated: false, map: false, proto_type: key_type };
    let value_field = ProtoField { message: field.message, json_name: "value", number: 2, repeated: false, map: false, proto_type: value_type };
    let mut key: Option<String> = None;
    let mut val: Value = Value::Null;
    let mut pos = 0;
    while pos < entry_bytes.len() {
        let (num, r) = wire::decode_field(entry_bytes, &mut pos)?;
        if num == 1 {
            key = Some(decode_one(message, &key_field, &r)?.as_str().unwrap_or_default().to_string());
        } else if num == 2 {
            val = decode_one(message, &value_field, &r)?;
        }
    }
    let mut obj = Map::new();
    obj.insert(key.unwrap_or_default(), val);
