
/// Encode `value` (a JSON object shaped like `message`'s fields) as a raw
/// protobuf message body — no envelope, no magic bytes. Use
/// [`wrap_unknown`] to produce the full wire payload the apiserver
/// actually sends.
pub fn encode_message(message: &str, value: &Value) -> Result<Vec<u8>> {
    include!("body-5-1.rs");
    include!("body-5-2.rs");
}
