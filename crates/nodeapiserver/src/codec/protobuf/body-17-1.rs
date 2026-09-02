    let raw = serde_json::to_vec(value).unwrap_or_default();
    let mut out = Vec::new();
    if !raw.is_empty() {
        wire::encode_tag(1, WireType::LengthDelimited, &mut out);
        wire::encode_length_delimited(&raw, &mut out);
    }
