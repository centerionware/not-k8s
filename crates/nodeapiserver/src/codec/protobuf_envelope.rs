/// Wraps an already-encoded object body in the full
/// `application/vnd.kubernetes.protobuf` wire payload: the 4-byte `k8s\0`
/// magic, then a length-delimited (implicit — this is the top-level
/// message, so no outer tag) `runtime.Unknown` whose `raw` field holds
/// `object_bytes` and whose `typeMeta` names `api_version`/`kind`.
pub fn wrap_unknown(api_version: &str, kind: &str, object_bytes: &[u8]) -> Vec<u8> {
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
    out
}
/// Unwraps a full wire payload back into `(api_version, kind, object_bytes)`
/// — `object_bytes` is still protobuf-encoded (as `encode_message`
/// produces), not yet decoded to JSON; call [`decode_message`] on it with
/// the schema resolved from `(api_version, kind)`.
pub fn unwrap_unknown(bytes: &[u8]) -> Result<(String, String, Vec<u8>)> {
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
    Ok((api_version, kind, object_bytes))
}

/// Which schema `resolve_message_ref` and the type-meta bridge both need:
/// looks up `(group, version, kind)` the way [`unwrap_unknown`]'s caller
/// would after splitting `apiVersion` into group/version.
pub fn schema_for_gvk(group: &str, version: &str, kind: &str) -> Option<&'static str> {
    codegen::schema_by_gvk().get(&(group, version, kind)).copied()
}
