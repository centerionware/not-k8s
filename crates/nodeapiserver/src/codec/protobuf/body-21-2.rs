    if ty == 1 {
        Ok(Value::String(str_val.unwrap_or_default()))
    } else {
        Ok(Value::from(int_val))
    }
