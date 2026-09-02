
/// Encodes an arbitrary JSON `value` as `JSON{raw: <value's own JSON
/// bytes>}` — infallible: `serde_json::Value`'s own `Serialize` impl
/// never itself produces a `serde_json::Error` (unlike parsing, which
/// can fail on malformed input, or serializing a type with a
/// hand-written fallible `Serialize`, `Value` is already fully validated
/// data). Omits the `raw` field entirely when `value` serializes to
/// nothing — can't happen for any real `serde_json::Value`, only kept as
/// "don't write a spurious empty tag" symmetry with every other optional
/// field this codec encodes.
fn encode_json_value(value: &Value) -> Vec<u8> {
    include!("body-17-1.rs");
    include!("body-17-2.rs");
}
