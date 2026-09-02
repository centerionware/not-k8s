    if field.map {
        return decode_map_entry(message, field, raw);
    }
    let label = || format!("{message}.{}", field.json_name);
