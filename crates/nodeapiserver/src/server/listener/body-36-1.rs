    if label_reqs.is_empty() && field_reqs.is_empty() {
        return true;
    }
    if event.value.is_empty() {
        return true;
    }
    let decoded = match storage {
        Some(s) => rest::decrypt_and_decode(s, group, resource, &event.key, &event.value),
        None => rest::decode_stored_object(&event.value).map_err(rest::Error::from),
    };
