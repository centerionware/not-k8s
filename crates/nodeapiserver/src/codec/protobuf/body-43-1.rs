    if bytes.len() < MAGIC.len() {
        return Err(Error::EnvelopeTooShort);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(Error::BadMagic);
    }
    let decoded = decode_message(UNKNOWN_MESSAGE, &bytes[MAGIC.len()..])?;
    let type_meta = decoded.get("typeMeta").cloned().unwrap_or(Value::Object(Map::new()));
    let api_version = type_meta.get("apiVersion").and_then(Value::as_str).unwrap_or_default().to_string();
    let kind = type_meta.get("kind").and_then(Value::as_str).unwrap_or_default().to_string();
    let raw_b64 = decoded.get("raw").and_then(Value::as_str).unwrap_or_default();
    let object_bytes = base64_decode(raw_b64).map_err(|e| Error::InvalidBase64("runtime.Unknown.raw".to_string(), e))?;
