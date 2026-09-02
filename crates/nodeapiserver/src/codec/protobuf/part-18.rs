
/// Decodes an `IntOrString{type, intVal, strVal}` message body back into
/// the plain scalar it represents — `type == 1` (String) means
/// `strVal`, anything else (including the field being entirely absent,
/// real upstream's own zero value) means `intVal` (defaulting to `0`,
/// matching a `nil`-equivalent `IntOrString{}`'s own real JSON
/// marshalling: `0`, not `null`).
fn decode_int_or_string_message(field_label: &str, bytes: &[u8]) -> Result<Value> {
    include!("body-21-1.rs");
    include!("body-21-2.rs");
}
