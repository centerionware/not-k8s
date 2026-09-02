    let mut seconds: i64 = 0;
    let mut nanos: i32 = 0;
    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        match field_number {
            1 => seconds = as_varint(field_label, &raw)? as i64,
            2 => nanos = as_varint(field_label, &raw)? as i32,
            _ => {}
        }
    }
    let dt = chrono::DateTime::from_timestamp(seconds, nanos as u32).ok_or_else(|| Error::InvalidTimestamp { field: field_label.to_string(), value: format!("seconds={seconds}, nanos={nanos}") })?;
