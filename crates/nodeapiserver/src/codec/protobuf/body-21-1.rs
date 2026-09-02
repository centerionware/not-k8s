    let mut ty: i64 = 0;
    let mut int_val: i32 = 0;
    let mut str_val: Option<String> = None;
    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        match field_number {
            1 => ty = as_varint(field_label, &raw)? as i64,
            2 => int_val = as_varint(field_label, &raw)? as u32 as i32,
            3 => str_val = Some(String::from_utf8_lossy(as_bytes(field_label, &raw)?).into_owned()),
            _ => {}
        }
    }
