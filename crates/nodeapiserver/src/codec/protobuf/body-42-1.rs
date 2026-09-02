    let mut type_meta = Map::new();
    type_meta.insert("apiVersion".to_string(), Value::String(api_version.to_string()));
    type_meta.insert("kind".to_string(), Value::String(kind.to_string()));

    let mut unknown = Map::new();
    unknown.insert("typeMeta".to_string(), Value::Object(type_meta));
    unknown.insert("raw".to_string(), Value::String(base64_encode(object_bytes)));

    let unknown_bytes = encode_message(UNKNOWN_MESSAGE, &Value::Object(unknown))
        .expect("encoding runtime.Unknown itself never fails: every field is a known scalar/message type");

    let mut out = Vec::with_capacity(MAGIC.len() + unknown_bytes.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&unknown_bytes);
