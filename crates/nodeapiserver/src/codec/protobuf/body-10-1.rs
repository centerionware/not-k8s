    if !codegen::proto_fields::PROTO_MESSAGES.contains(&message) {
        return Err(Error::UnknownMessage(message.to_string()));
    }
    let by_number = fields_by_number(message);
    let mut obj = Map::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        let Some(field) = by_number.get(&field_number) else {
            // Unknown field number for this message — same
            // forward-compatibility posture as the encoder: skip rather
            // than fail, we've already consumed exactly its bytes via
            // decode_field's own wire-type-aware length handling.
            continue;
        };
        let decoded = decode_one(message, field, &raw)?;
        if is_inline_embedded_field(message, field.json_name) {
            // Real upstream Go-struct-embedding (`is_inline_embedded_field`'s
            // own doc comment): the nested message's own fields belong
            // flattened directly into this same object, not nested under
            // a `ruleWithOperations`/`rule` wrapper key JSON never has.
            let Value::Object(nested) = decoded else { unreachable!("an embedded field always decodes to a JSON object") };
            obj.extend(nested);
        } else if field.map {
            let Value::Object(map) = obj.entry(field.json_name).or_insert_with(|| Value::Object(Map::new())) else {
                unreachable!("map field always inserts a JSON object");
            };
            let Value::Object(entry) = decoded else { unreachable!("map entry decodes to a one-key object") };
            map.extend(entry);
        } else if field.repeated {
            let Value::Array(arr) = obj.entry(field.json_name).or_insert_with(|| Value::Array(Vec::new())) else {
                unreachable!("repeated field always inserts a JSON array");
            };
            arr.push(decoded);
        } else {
            obj.insert(field.json_name.to_string(), decoded);
        }
    }
