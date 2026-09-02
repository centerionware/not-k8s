    let dt = chrono::DateTime::parse_from_rfc3339(s).map_err(|_| Error::InvalidTimestamp { field: field_label.to_string(), value: s.to_string() })?;
    let mut out = Vec::new();
    let seconds = dt.timestamp();
    let nanos = dt.timestamp_subsec_nanos() as i32;
    if seconds != 0 {
        wire::encode_tag(1, WireType::Varint, &mut out);
        wire::encode_varint_i64(seconds, &mut out);
    }
    if nanos != 0 {
        wire::encode_tag(2, WireType::Varint, &mut out);
        wire::encode_varint_i32(nanos, &mut out);
    }
