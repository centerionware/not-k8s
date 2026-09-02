
/// Decodes a `JSON{raw: bytes}` message body back into the JSON value it
/// wraps. No `raw` field present at all is real upstream's own zero
/// value for the message (an operator's schema simply didn't set this
/// particular literal) — `Value::Null`, matching what a `nil` `apiextensions.JSON`
/// marshals to in Go, not an error.
fn decode_json_message(field_label: &str, bytes: &[u8]) -> Result<Value> {
    include!("body-18-1.rs");
    include!("body-18-2.rs");
}
