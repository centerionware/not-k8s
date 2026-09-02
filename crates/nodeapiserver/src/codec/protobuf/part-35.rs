
/// Unwraps a full wire payload back into `(api_version, kind, object_bytes)`
/// — `object_bytes` is still protobuf-encoded (as `encode_message`
/// produces), not yet decoded to JSON; call [`decode_message`] on it with
/// the schema resolved from `(api_version, kind)`.
pub fn unwrap_unknown(bytes: &[u8]) -> Result<(String, String, Vec<u8>)> {
    include!("body-43-1.rs");
    include!("body-43-2.rs");
}
