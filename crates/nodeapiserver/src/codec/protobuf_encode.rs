pub fn encode_message(message: &str, value: &Value) -> Result<Vec<u8>> {
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
    Ok(out)
}
/// Real upstream Go struct embedding: a field declared as `Foo \`json:",inline"\``
/// has every one of `Foo`'s own fields flattened directly into the
/// *enclosing* JSON object with no wrapper key at all, while the
/// generated proto keeps it as an ordinary named nested-message field
/// (`optional Foo foo = N`) — confirmed directly against the vendored
/// `.proto` (`NamedRuleWithOperations.ruleWithOperations` and
/// `RuleWithOperations.rule`, both in
/// `vendor/protos/k8s.io/api/admissionregistration/v1/generated.proto`,
/// matching real upstream's own `k8s.io/api/admissionregistration/v1/types.go`
/// struct embedding). The core/v1 `Volume.volumeSource`,
/// `PersistentVolumeSpec.persistentVolumeSource`, and
/// `EphemeralContainer.ephemeralContainerCommon` fields have the same
/// flattened JSON shape, as do the core/v1 `LocalObjectReference` fields
/// in config-map and secret sources/selectors. Found live:
/// `ValidatingAdmissionPolicy`'s own
/// `spec.matchConstraints.resourceRules[]` round-tripped through a real
/// `nodestore` as entirely empty objects (every field but
/// `resourceNames` silently dropped) until this was special-cased —
/// every other message type in this codec really is just "recurse with
/// the same field-shaped JSON object", this is the one place two levels
/// of real upstream embedding needed a named exception.
fn is_inline_embedded_field(message: &str, json_name: &str) -> bool {
    // v1alpha1/v1beta1's own `NamedRuleWithOperations.ruleWithOperations`
    // both reference `v1`'s `RuleWithOperations` directly (confirmed in
    // the vendored proto -- neither version has its own copy of that
    // message), so a single `v1` entry for the inner field covers every
    // API version.
    matches!(
        (message, json_name),
        ("io.k8s.api.admissionregistration.v1.NamedRuleWithOperations", "ruleWithOperations")
            | ("io.k8s.api.admissionregistration.v1beta1.NamedRuleWithOperations", "ruleWithOperations")
            | ("io.k8s.api.admissionregistration.v1alpha1.NamedRuleWithOperations", "ruleWithOperations")
            | ("io.k8s.api.admissionregistration.v1.RuleWithOperations", "rule")
            | ("io.k8s.api.core.v1.Volume", "volumeSource")
            | ("io.k8s.api.core.v1.PersistentVolumeSpec", "persistentVolumeSource")
            | ("io.k8s.api.core.v1.EphemeralContainer", "ephemeralContainerCommon")
            | ("io.k8s.api.core.v1.ConfigMapEnvSource", "localObjectReference")
            | ("io.k8s.api.core.v1.ConfigMapKeySelector", "localObjectReference")
            | ("io.k8s.api.core.v1.ConfigMapProjection", "localObjectReference")
            | ("io.k8s.api.core.v1.ConfigMapVolumeSource", "localObjectReference")
            | ("io.k8s.api.core.v1.SecretEnvSource", "localObjectReference")
            | ("io.k8s.api.core.v1.SecretKeySelector", "localObjectReference")
            | ("io.k8s.api.core.v1.SecretProjection", "localObjectReference")
            | ("io.k8s.api.core.v1.SecretVolumeSource", "localObjectReference")
            | ("io.k8s.api.core.v1.Probe", "handler")
    )
}

fn encode_field(message: &str, field: &ProtoField, value: &Value, out: &mut Vec<u8>) -> Result<()> {
    if field.map {
        return encode_map_field(message, field, value, out);
    }
    if field.repeated {
        let Value::Array(items) = value else {
            return Err(type_mismatch(message, field, "array", value));
        };
        for item in items {
            encode_scalar_or_message(message, field, item, out)?;
        }
        return Ok(());
    }
    encode_scalar_or_message(message, field, value, out)
}

/// Encodes one value (a single element of a repeated field, or a
/// non-repeated field's whole value) as one wire field: tag, then payload.
fn encode_scalar_or_message(message: &str, field: &ProtoField, value: &Value, out: &mut Vec<u8>) -> Result<()> {
    match ScalarKind::of(&field.proto_type) {
        Some(ScalarKind::Bool) => {
            let b = value.as_bool().ok_or_else(|| type_mismatch(message, field, "bool", value))?;
            wire::encode_tag(field.number, WireType::Varint, out);
            wire::encode_varint(b as u64, out);
        }
        Some(ScalarKind::Int32) => {
            let n = value.as_i64().ok_or_else(|| type_mismatch(message, field, "int32", value))?;
            wire::encode_tag(field.number, WireType::Varint, out);
            wire::encode_varint_i32(n as i32, out);
        }
        Some(ScalarKind::Int64) => {
            let n = value.as_i64().ok_or_else(|| type_mismatch(message, field, "int64", value))?;
            wire::encode_tag(field.number, WireType::Varint, out);
            wire::encode_varint_i64(n, out);
        }
        Some(ScalarKind::Double) => {
            let n = value.as_f64().ok_or_else(|| type_mismatch(message, field, "double", value))?;
            wire::encode_tag(field.number, WireType::Fixed64, out);
            wire::encode_fixed64(n, out);
        }
        Some(ScalarKind::String) => {
            let s = value.as_str().ok_or_else(|| type_mismatch(message, field, "string", value))?;
            wire::encode_tag(field.number, WireType::LengthDelimited, out);
            wire::encode_length_delimited(s.as_bytes(), out);
        }
        Some(ScalarKind::Bytes) => {
            let s = value.as_str().ok_or_else(|| type_mismatch(message, field, "base64 string", value))?;
            let bytes = base64_decode(s).map_err(|e| Error::InvalidBase64(format!("{message}.{}", field.json_name), e))?;
            wire::encode_tag(field.number, WireType::LengthDelimited, out);
            wire::encode_length_delimited(&bytes, out);
        }
        None => {
            let nested_message = codegen::resolve_message_ref(message, &field.proto_type);
            let nested_bytes = if is_time_message(&nested_message) {
                // The one genuinely irreducible exception to this
                // encoder's otherwise fully generic reflection-based
                // approach — see `encode_time_string`'s own doc comment.
                let s = value.as_str().ok_or_else(|| type_mismatch(message, field, "RFC3339 timestamp string", value))?;
                encode_time_string(&format!("{message}.{}", field.json_name), s)?
            } else if is_fields_v1_message(&nested_message) {
                // FieldsV1 is a JSON-shaped field set wrapped in a single
                // protobuf `Raw` bytes field, just like the other dynamic
                // JSON messages below. Treating its `f:*` keys as ordinary
                // protobuf fields silently drops managed-field ownership on
                // every protobuf storage round trip.
                encode_json_value(value)
            } else if is_json_message(&nested_message) {
                // Group K: `apiextensions.v1.JSON` — see `is_json_message`'s
                // own doc comment.
                encode_json_value(value)
            } else if is_json_schema_props_or_array(&nested_message) {
                encode_json_schema_props_or_array(&nested_message, value)?
            } else if is_json_schema_props_or_bool(&nested_message) {
                encode_json_schema_props_or_bool(&nested_message, value)?
            } else if is_int_or_string_message(&nested_message) {
                encode_int_or_string(message, field, value)?
            } else if is_quantity_message(&nested_message) {
                encode_quantity(message, field, value)?
            } else {
                encode_message(&nested_message, value)?
            };
            wire::encode_tag(field.number, WireType::LengthDelimited, out);
            wire::encode_length_delimited(&nested_bytes, out);
        }
    }
    Ok(())
}

/// `map<K, V>` is encoded on the wire as `repeated` of a synthetic
/// two-field entry message: `key = 1` (always `string` in the vendored
/// set — confirmed by grep), `value = 2`. One such entry per JSON object
/// property, each independently length-delimited and tagged with the map
/// field's own number.
fn encode_map_field(message: &str, field: &ProtoField, value: &Value, out: &mut Vec<u8>) -> Result<()> {
    let Value::Object(entries) = value else {
        return Err(type_mismatch(message, field, "object (map)", value));
    };
    let (key_type, value_type) = split_map_type(&field.proto_type)?;
    for (k, v) in entries {
        let mut entry = Vec::new();
        let key_field = ProtoField { message: field.message, json_name: "key", number: 1, repeated: false, map: false, proto_type: key_type };
        encode_scalar_or_message(message, &key_field, &Value::String(k.clone()), &mut entry)?;
        let value_field = ProtoField { message: field.message, json_name: "value", number: 2, repeated: false, map: false, proto_type: value_type };
        encode_scalar_or_message(message, &value_field, v, &mut entry)?;
        wire::encode_tag(field.number, WireType::LengthDelimited, out);
        wire::encode_length_delimited(&entry, out);
    }
    Ok(())
}
