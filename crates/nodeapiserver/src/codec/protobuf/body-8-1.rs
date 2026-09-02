    match ScalarKind::of(&field.proto_type) {
        Some(ScalarKind::Bool) => {
            let b = value.as_bool().ok_or_else(|| type_mismatch(message, field, "bool", value))?;
            wire::encode_tag(field.number, WireType::Varint, out);
            wire::encode_varint(b as u64, out);
        }
        Some(ScalarKind::Int32) => {
            let n = value.as_i64().ok_or_else(|| type_mismatch(message, field, "int32", value))?;
            wire::encode_tag(field.number, WireType::Varint, out);
            wire::encode_varint_i32(n as i32, out);
        }
        Some(ScalarKind::Int64) => {
            let n = value.as_i64().ok_or_else(|| type_mismatch(message, field, "int64", value))?;
            wire::encode_tag(field.number, WireType::Varint, out);
            wire::encode_varint_i64(n, out);
        }
        Some(ScalarKind::Double) => {
            let n = value.as_f64().ok_or_else(|| type_mismatch(message, field, "double", value))?;
            wire::encode_tag(field.number, WireType::Fixed64, out);
            wire::encode_fixed64(n, out);
        }
        Some(ScalarKind::String) => {
            let s = value.as_str().ok_or_else(|| type_mismatch(message, field, "string", value))?;
            wire::encode_tag(field.number, WireType::LengthDelimited, out);
            wire::encode_length_delimited(s.as_bytes(), out);
        }
        Some(ScalarKind::Bytes) => {
            let s = value.as_str().ok_or_else(|| type_mismatch(message, field, "base64 string", value))?;
            let bytes = base64_decode(s).map_err(|e| Error::InvalidBase64(format!("{message}.{}", field.json_name), e))?;
            wire::encode_tag(field.number, WireType::LengthDelimited, out);
            wire::encode_length_delimited(&bytes, out);
        }
        None => {
            let nested_message = codegen::resolve_message_ref(message, &field.proto_type);
            let nested_bytes = if is_time_message(&nested_message) {
                // The one genuinely irreducible exception to this
                // encoder's otherwise fully generic reflection-based
                // approach — see `encode_time_string`'s own doc comment.
                let s = value.as_str().ok_or_else(|| type_mismatch(message, field, "RFC3339 timestamp string", value))?;
                encode_time_string(&format!("{message}.{}", field.json_name), s)?
            } else if is_json_message(&nested_message) {
                // Group K: `apiextensions.v1.JSON` — see `is_json_message`'s
                // own doc comment.
                encode_json_value(value)
            } else if is_json_schema_props_or_array(&nested_message) {
                encode_json_schema_props_or_array(&nested_message, value)?
            } else if is_json_schema_props_or_bool(&nested_message) {
                encode_json_schema_props_or_bool(&nested_message, value)?
            } else if is_int_or_string_message(&nested_message) {
                encode_int_or_string(message, field, value)?
            } else if is_quantity_message(&nested_message) {
                encode_quantity(message, field, value)?
            } else {
                encode_message(&nested_message, value)?
            };
            wire::encode_tag(field.number, WireType::LengthDelimited, out);
            wire::encode_length_delimited(&nested_bytes, out);
        }
    }
