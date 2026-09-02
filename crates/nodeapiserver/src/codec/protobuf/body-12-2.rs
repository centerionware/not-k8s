    match ScalarKind::of(&field.proto_type) {
        Some(ScalarKind::Bool) => Ok(Value::Bool(as_varint(&label(), raw)? != 0)),
        Some(ScalarKind::Int32) => Ok(Value::from(as_varint(&label(), raw)? as u32 as i32)),
        Some(ScalarKind::Int64) => Ok(Value::from(as_varint(&label(), raw)? as i64)),
        Some(ScalarKind::Double) => {
            Ok(serde_json::Number::from_f64(as_fixed64(&label(), raw)?).map(Value::Number).unwrap_or(Value::Null))
        }
        Some(ScalarKind::String) => Ok(Value::String(String::from_utf8_lossy(as_bytes(&label(), raw)?).into_owned())),
        Some(ScalarKind::Bytes) => Ok(Value::String(base64_encode(as_bytes(&label(), raw)?))),
        None => {
            let nested_message = codegen::resolve_message_ref(message, &field.proto_type);
            if is_time_message(&nested_message) {
                decode_time_message(&label(), as_bytes(&label(), raw)?)
            } else if is_json_message(&nested_message) {
                decode_json_message(&label(), as_bytes(&label(), raw)?)
            } else if is_json_schema_props_or_array(&nested_message) {
                decode_json_schema_props_or_array(&nested_message, as_bytes(&label(), raw)?)
            } else if is_json_schema_props_or_bool(&nested_message) {
                decode_json_schema_props_or_bool(&nested_message, as_bytes(&label(), raw)?)
            } else if is_int_or_string_message(&nested_message) {
                decode_int_or_string_message(&label(), as_bytes(&label(), raw)?)
            } else if is_quantity_message(&nested_message) {
                decode_quantity_message(&label(), as_bytes(&label(), raw)?)
            } else {
                decode_message(&nested_message, as_bytes(&label(), raw)?)
            }
        }
    }
