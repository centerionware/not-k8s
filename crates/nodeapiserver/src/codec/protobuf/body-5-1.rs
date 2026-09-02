    let Value::Object(obj) = value else {
        return Err(Error::NotAnObject(message.to_string()));
    };
    let mut out = Vec::new();
    for (field_name, field_value) in obj {
        if field_value.is_null() {
            // proto2 `optional` fields simply aren't written when absent —
            // there is no "explicit null" on the wire to distinguish from
            // "not set", so a JSON null is the same as an absent key.
            continue;
        }
        let Some(field) = codegen::proto_field_index().get(&(message, field_name.as_str())) else {
            // Unknown field for this message — most likely a field this
            // vendored release doesn't have, or a client sending something
            // newer than this build knows about. Skipping it (rather than
            // erroring) matches protobuf's own forward-compatibility
            // posture: an unrecognized field is silently dropped, not a
            // hard failure.
            continue;
        };
        encode_field(message, field, field_value, &mut out)?;
    }
    // Real upstream Go-struct-embedding fields (`inline_embedded_fields`'s
    // own doc comment): JSON has no wrapper key for these at all, so the
    // loop above never matches them by name. Encode each one using this
    // same outer object again -- `encode_message`'s own recursion (via
    // `encode_scalar_or_message`) picks out only the keys the nested
    // message actually declares, so this is safe even when several
    // embedded levels chain (`NamedRuleWithOperations` -> `RuleWithOperations`
    // -> `Rule`) or when the object doesn't carry every nested field.
    for field in codegen::proto_fields::PROTO_FIELDS.iter().filter(|f| f.message == message) {
        if is_inline_embedded_field(message, field.json_name) {
            encode_field(message, field, value, &mut out)?;
        }
    }
