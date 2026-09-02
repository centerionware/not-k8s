    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        if field_number == 1 {
            return serde_json::from_slice(as_bytes(field_label, &raw)?).map_err(Error::Json);
        }
    }
