
/// Decode a raw protobuf message body (no envelope) into a JSON object
/// shaped like `message`'s fields.
pub fn decode_message(message: &str, bytes: &[u8]) -> Result<Value> {
    include!("body-10-1.rs");
    include!("body-10-2.rs");
}
