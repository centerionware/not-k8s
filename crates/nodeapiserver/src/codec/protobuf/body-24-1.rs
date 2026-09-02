    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        if field_number == 1 {
            return Ok(Value::String(String::from_utf8_lossy(as_bytes(field_label, &raw)?).into_owned()));
        }
    }
