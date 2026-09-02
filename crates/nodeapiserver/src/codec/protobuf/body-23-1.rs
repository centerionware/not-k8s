    let s = value.as_str().ok_or_else(|| type_mismatch(message, field, "a quantity string", value))?;
    let mut out = Vec::new();
    if !s.is_empty() {
        wire::encode_tag(1, WireType::LengthDelimited, &mut out);
        wire::encode_length_delimited(s.as_bytes(), &mut out);
    }
