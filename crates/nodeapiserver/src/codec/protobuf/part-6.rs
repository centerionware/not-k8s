
/// `map<K, V>` is encoded on the wire as `repeated` of a synthetic
/// two-field entry message: `key = 1` (always `string` in the vendored
/// set — confirmed by grep), `value = 2`. One such entry per JSON object
/// property, each independently length-delimited and tagged with the map
/// field's own number.
fn encode_map_field(message: &str, field: &ProtoField, value: &Value, out: &mut Vec<u8>) -> Result<()> {
    include!("body-9-1.rs");
    include!("body-9-2.rs");
}
