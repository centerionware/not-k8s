
/// Encodes one value (a single element of a repeated field, or a
/// non-repeated field's whole value) as one wire field: tag, then payload.
fn encode_scalar_or_message(message: &str, field: &ProtoField, value: &Value, out: &mut Vec<u8>) -> Result<()> {
    include!("body-8-1.rs");
    include!("body-8-2.rs");
}
