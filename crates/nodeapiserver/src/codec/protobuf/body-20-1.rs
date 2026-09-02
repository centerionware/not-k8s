    let mut out = Vec::new();
    if let Some(n) = value.as_i64() {
        wire::encode_tag(2, WireType::Varint, &mut out);
        wire::encode_varint_i32(n as i32, &mut out);
    } else if let Some(s) = value.as_str() {
        wire::encode_tag(1, WireType::Varint, &mut out);
        wire::encode_varint_i64(1, &mut out);
        wire::encode_tag(3, WireType::LengthDelimited, &mut out);
        wire::encode_length_delimited(s.as_bytes(), &mut out);
    } else {
        return Err(type_mismatch(message, field, "an int or a string (IntOrString)", value));
    }
