
fn fields_by_number(message: &str) -> std::collections::HashMap<u32, &'static ProtoField> {
    codegen::proto_fields::PROTO_FIELDS
        .iter()
        .filter(|f| f.message == message)
        .map(|f| (f.number, f))
        .collect()
}
