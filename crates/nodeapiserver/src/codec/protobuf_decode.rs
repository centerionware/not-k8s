/// Decode a raw protobuf message body (no envelope) into a JSON object
/// shaped like `message`'s fields.
pub fn decode_message(message: &str, bytes: &[u8]) -> Result<Value> {
    if !codegen::proto_message_set().contains(message) {
        return Err(Error::UnknownMessage(message.to_string()));
    }
    let by_number = codegen::proto_field_index_by_number();
    let mut obj = Map::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        let Some(field) = by_number.get(&(message, field_number)) else {
            // Unknown field number for this message — same
            // forward-compatibility posture as the encoder: skip rather
            // than fail, we've already consumed exactly its bytes via
            // decode_field's own wire-type-aware length handling.
            continue;
        };
        let decoded = decode_one(message, field, &raw)?;
        if is_inline_embedded_field(message, field.json_name) {
            // Real upstream Go-struct-embedding (`is_inline_embedded_field`'s
            // own doc comment): the nested message's own fields belong
            // flattened directly into this same object, not nested under
            // a `ruleWithOperations`/`rule` wrapper key JSON never has.
            let Value::Object(nested) = decoded else {
                unreachable!("an embedded field always decodes to a JSON object")
            };
            obj.extend(nested);
        } else if field.map {
            let Value::Object(map) = obj
                .entry(field.json_name)
                .or_insert_with(|| Value::Object(Map::new()))
            else {
                unreachable!("map field always inserts a JSON object");
            };
            let Value::Object(entry) = decoded else {
                unreachable!("map entry decodes to a one-key object")
            };
            map.extend(entry);
        } else if field.repeated {
            let Value::Array(arr) = obj
                .entry(field.json_name)
                .or_insert_with(|| Value::Array(Vec::new()))
            else {
                unreachable!("repeated field always inserts a JSON array");
            };
            arr.push(decoded);
        } else {
            obj.insert(field.json_name.to_string(), decoded);
        }
    }
    Ok(Value::Object(obj))
}
fn decode_one(message: &str, field: &ProtoField, raw: &RawField) -> Result<Value> {
    if field.map {
        return decode_map_entry(message, field, raw);
    }
    let label = || format!("{message}.{}", field.json_name);
    match ScalarKind::of(&field.proto_type) {
        Some(ScalarKind::Bool) => Ok(Value::Bool(as_varint(&label(), raw)? != 0)),
        Some(ScalarKind::Int32) => Ok(Value::from(as_varint(&label(), raw)? as u32 as i32)),
        Some(ScalarKind::Int64) => Ok(Value::from(as_varint(&label(), raw)? as i64)),
        Some(ScalarKind::Double) => Ok(serde_json::Number::from_f64(as_fixed64(&label(), raw)?)
            .map(Value::Number)
            .unwrap_or(Value::Null)),
        Some(ScalarKind::String) => Ok(Value::String(
            String::from_utf8_lossy(as_bytes(&label(), raw)?).into_owned(),
        )),
        Some(ScalarKind::Bytes) => Ok(Value::String(base64_encode(as_bytes(&label(), raw)?))),
        None => {
            let nested_message = codegen::resolve_message_ref(message, &field.proto_type);
            if is_time_message(&nested_message) {
                decode_time_message(&nested_message, &label(), as_bytes(&label(), raw)?)
            } else if is_fields_v1_message(&nested_message) {
                decode_json_message(&label(), as_bytes(&label(), raw)?)
            } else if is_json_message(&nested_message) {
                decode_json_message(&label(), as_bytes(&label(), raw)?)
            } else if is_json_schema_props_or_array(&nested_message) {
                decode_json_schema_props_or_array(&nested_message, as_bytes(&label(), raw)?)
            } else if is_json_schema_props_or_bool(&nested_message) {
                decode_json_schema_props_or_bool(&nested_message, as_bytes(&label(), raw)?)
            } else if is_int_or_string_message(&nested_message) {
                decode_int_or_string_message(&label(), as_bytes(&label(), raw)?)
            } else if is_quantity_message(&nested_message) {
                decode_quantity_message(&label(), as_bytes(&label(), raw)?)
            } else {
                decode_message(&nested_message, as_bytes(&label(), raw)?)
            }
        }
    }
}

/// Real upstream's own well-known-type special case, confirmed directly
/// against the vendored proto rather than guessed at: `metav1.Time`
/// (`staging/src/k8s.io/apimachinery/pkg/apis/meta/v1/generated.proto`'s
/// own `message Time`) is represented in JSON as a plain RFC3339 string
/// (`creationTimestamp: "2024-01-01T00:00:00Z"`), but its vendored proto
/// message wraps `{seconds: int64 = 1, nanos: int32 = 2}` — the same
/// shape `google.protobuf.Timestamp` uses — because real upstream's own
/// Go marshaller (`Time.MarshalTo`/`Time.Unmarshal`) hand-converts
/// between the two. `MicroTime` uses the identical wire shape. This is
/// the *only* place this otherwise fully generic, reflection-driven
/// encoder needs a named exception — every other message type really is
/// just "recurse with the same field-shaped JSON object" the rest of
/// this module assumes. Found live: nothing exercised this crate's own
/// `encode_message`/`decode_message` against a real object carrying a
/// real `creationTimestamp` end to end (through an actual protobuf
/// round trip against a real datastore) until
/// `tests/encryption_roundtrip.rs`'s own live round trip did — every
/// prior test either stayed at the JSON/YAML codec layer (which never
/// hits this at all) or unit-tested `encode_message`/`decode_message`
/// with hand-built fixtures that happened never to include a `Time`
/// field.
fn is_time_message(message: &str) -> bool {
    matches!(
        message,
        "io.k8s.apimachinery.pkg.apis.meta.v1.Time"
            | "io.k8s.apimachinery.pkg.apis.meta.v1.MicroTime"
    )
}

fn encode_time_string(field_label: &str, s: &str) -> Result<Vec<u8>> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).map_err(|_| Error::InvalidTimestamp {
        field: field_label.to_string(),
        value: s.to_string(),
    })?;
    let mut out = Vec::new();
    let seconds = dt.timestamp();
    let nanos = dt.timestamp_subsec_nanos() as i32;
    if seconds != 0 {
        wire::encode_tag(1, WireType::Varint, &mut out);
        wire::encode_varint_i64(seconds, &mut out);
    }
    if nanos != 0 {
        wire::encode_tag(2, WireType::Varint, &mut out);
        wire::encode_varint_i32(nanos, &mut out);
    }
    Ok(out)
}

fn decode_time_message(message: &str, field_label: &str, bytes: &[u8]) -> Result<Value> {
    let mut seconds: i64 = 0;
    let mut nanos: i32 = 0;
    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        match field_number {
            1 => seconds = as_varint(field_label, &raw)? as i64,
            2 => nanos = as_varint(field_label, &raw)? as i32,
            _ => {}
        }
    }
    let dt = chrono::DateTime::from_timestamp(seconds, nanos as u32).ok_or_else(|| {
        Error::InvalidTimestamp {
            field: field_label.to_string(),
            value: format!("seconds={seconds}, nanos={nanos}"),
        }
    })?;
    let format = if message.ends_with("MicroTime") {
        chrono::SecondsFormat::Micros
    } else {
        chrono::SecondsFormat::AutoSi
    };
    Ok(Value::String(dt.to_rfc3339_opts(format, true)))
}

/// Group K's own well-known-type special case, the same shape
/// `is_time_message` already established and confirmed directly against
/// the vendored proto rather than guessed: `apiextensions.v1.JSON`
/// (`vendor/protos/k8s.io/apiextensions-apiserver/pkg/apis/apiextensions/
/// v1/generated.proto`'s own `message JSON { optional bytes raw = 1; }`)
/// is how a `CustomResourceDefinition`'s own schema represents an
/// arbitrary JSON value — `JSONSchemaProps.default`/`.example`/`.enum`/
/// `.const`, wherever an operator's own schema can name a literal value
/// of any shape. In JSON the field is just that value directly (a
/// `default: "small"` looks exactly like every other scalar field), but
/// on the wire it's this one-field wrapper message whose `raw` holds the
/// value's own JSON encoding as bytes — real upstream's Go type
/// (`apiextensions.JSON`, a `[]byte` with hand-written `MarshalJSON`/
/// `UnmarshalJSON`) does the identical two-faced trick `metav1.Time`
/// does, just wrapping a whole JSON document instead of a timestamp.
/// Found live, the same way `is_time_message` was: nothing exercised a
/// real `CustomResourceDefinition` with a schema `default` through this
/// crate's own protobuf codec until `tests/crd_roundtrip.rs`'s live
/// round trip did.
/// `runtime.RawExtension`
/// (`vendor/protos/k8s.io/apimachinery/pkg/runtime/generated.proto`'s own
/// `message RawExtension { optional bytes raw = 1; }`, confirmed
/// directly) shares the *exact same* `{raw: bytes = 1}` wire shape and
/// "the whole JSON value lives in this one wrapped field" semantics as
/// `apiextensions.v1.JSON` above — real upstream's own `RawExtension.
/// MarshalJSON`/`UnmarshalJSON` store/emit `Raw` as the literal embedded
/// JSON document, not a base64 `bytes` field, identically to `JSON.Raw`.
/// Found via a deliberate audit pass (not a live failure this time):
/// after finding four separate real bugs of this exact class live this
/// session (`Time`, `apiextensions.v1.JSON`, `JSONSchemaPropsOrArray`/
/// `OrBool`, `IntOrString`), checking the vendored protos for
/// `RawExtension` ahead of the next real object that happens to carry
/// one (`Event.regarding`... no — `AdmissionReview.request.object`,
/// `WatchEvent`-adjacent dynamic fields, CRD conversion webhook payloads,
/// anywhere upstream models "an arbitrary embedded object"). Reuses
/// `encode_json_value`/`decode_json_message` directly — no new function
/// needed, since the wire shape and JSON semantics are identical.
fn is_json_message(message: &str) -> bool {
    matches!(
        message,
        "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSON"
            | "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1beta1.JSON"
            | "io.k8s.apimachinery.pkg.runtime.RawExtension"
    )
}

/// `metav1.FieldsV1` has a custom JSON representation: its arbitrary field
/// set is carried in the protobuf `Raw` bytes member rather than as ordinary
/// protobuf properties. This is the managed-fields counterpart to
/// `apiextensions.v1.JSON` and `runtime.RawExtension`.
fn is_fields_v1_message(message: &str) -> bool {
    message == "io.k8s.apimachinery.pkg.apis.meta.v1.FieldsV1"
}

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
    let raw = serde_json::to_vec(value).unwrap_or_default();
    let mut out = Vec::new();
    if !raw.is_empty() {
        wire::encode_tag(1, WireType::LengthDelimited, &mut out);
        wire::encode_length_delimited(&raw, &mut out);
    }
    out
}

/// Decodes a `JSON{raw: bytes}` message body back into the JSON value it
/// wraps. No `raw` field present at all is real upstream's own zero
/// value for the message (an operator's schema simply didn't set this
/// particular literal) — `Value::Null`, matching what a `nil` `apiextensions.JSON`
/// marshals to in Go, not an error.
fn decode_json_message(field_label: &str, bytes: &[u8]) -> Result<Value> {
    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        if field_number == 1 {
            return serde_json::from_slice(as_bytes(field_label, &raw)?).map_err(Error::Json);
        }
    }
    Ok(Value::Null)
}

/// Real upstream's own `intstr.IntOrString`
/// (`vendor/protos/k8s.io/apimachinery/pkg/util/intstr/generated.proto`'s
/// own `message IntOrString { optional int64 type = 1; optional int32
/// intVal = 2; optional string strVal = 3; }`, confirmed directly) — the
/// *fourth* occurrence of the same real `is_time_message` pattern: a Go
/// type (`intstr.IntOrString`) with hand-written `MarshalJSON`/
/// `UnmarshalJSON` that produces/consumes a plain scalar (a bare number
/// or a bare string — `targetPort: 8080` or `targetPort: "https"` are
/// both real, valid JSON for this field) rather than its own compiled
/// struct shape. Used all over the vendored schema wherever a field can
/// name either a numeric or a named value (`Service.spec.ports[].
/// targetPort`, `NetworkPolicyPort.port`, ...). **Found live**, the same
/// way every prior occurrence was: nothing exercised a real object
/// carrying a real `IntOrString` field through this codec's own protobuf
/// round trip until `tests/aggregator_proxy_roundtrip.rs`'s live
/// `Service.spec.ports[].targetPort` did — every prior test either
/// stayed at the JSON/YAML codec layer or happened not to submit an
/// object with this field populated.
fn is_int_or_string_message(message: &str) -> bool {
    message == "io.k8s.apimachinery.pkg.util.intstr.IntOrString"
}

/// Real upstream's own `Type` discriminator convention (`intstr.Int = 0`,
/// `intstr.String = 1`, confirmed directly against
/// `pkg/util/intstr/intstr.go`): a JSON number encodes as `{type: 0,
/// intVal: N}` (`type: 0` is proto2 `optional`'s own zero value, so
/// nothing is written for it — same "omit an explicit zero" posture
/// every other optional scalar field in this codec already takes), a
/// JSON string as `{type: 1, strVal: S}`.
fn encode_int_or_string(message: &str, field: &ProtoField, value: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if let Some(n) = value.as_i64() {
        wire::encode_tag(2, WireType::Varint, &mut out);
        wire::encode_varint_i32(n as i32, &mut out);
    } else if let Some(s) = value.as_str() {
        wire::encode_tag(1, WireType::Varint, &mut out);
        wire::encode_varint_i64(1, &mut out);
        wire::encode_tag(3, WireType::LengthDelimited, &mut out);
        wire::encode_length_delimited(s.as_bytes(), &mut out);
    } else {
        return Err(type_mismatch(
            message,
            field,
            "an int or a string (IntOrString)",
            value,
        ));
    }
    Ok(out)
}

/// Decodes an `IntOrString{type, intVal, strVal}` message body back into
/// the plain scalar it represents — `type == 1` (String) means
/// `strVal`, anything else (including the field being entirely absent,
/// real upstream's own zero value) means `intVal` (defaulting to `0`,
/// matching a `nil`-equivalent `IntOrString{}`'s own real JSON
/// marshalling: `0`, not `null`).
fn decode_int_or_string_message(field_label: &str, bytes: &[u8]) -> Result<Value> {
    let mut ty: i64 = 0;
    let mut int_val: i32 = 0;
    let mut str_val: Option<String> = None;
    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        match field_number {
            1 => ty = as_varint(field_label, &raw)? as i64,
            2 => int_val = as_varint(field_label, &raw)? as u32 as i32,
            3 => str_val = Some(String::from_utf8_lossy(as_bytes(field_label, &raw)?).into_owned()),
            _ => {}
        }
    }
    if ty == 1 {
        Ok(Value::String(str_val.unwrap_or_default()))
    } else {
        Ok(Value::from(int_val))
    }
}

/// `resource.Quantity`/`resource.QuantityValue`
/// (`vendor/protos/k8s.io/apimachinery/pkg/api/resource/generated.proto`'s
/// own `message Quantity { optional string string = 1; }` — both
/// messages share the identical one-field shape, confirmed directly) —
/// the *fifth* real occurrence of the same well-known-type pattern,
/// found via the same deliberate audit pass that found `RawExtension`
/// above: real upstream's own `+protobuf.embed=string` annotation plus a
/// hand-written `MarshalJSON`/`UnmarshalJSON` (delegating to `Quantity.
/// String()`) means the JSON representation is a bare string
/// (`"100m"`, `"1.5Gi"`), never `{"string": "100m"}`. Reused directly by
/// `scheme::quantity::Quantity`'s own string grammar at the JSON/YAML
/// codec layer already — this is the same value's *protobuf* wire
/// shape, a separate layer nothing had exercised live yet.
fn is_quantity_message(message: &str) -> bool {
    matches!(
        message,
        "io.k8s.apimachinery.pkg.api.resource.Quantity"
            | "io.k8s.apimachinery.pkg.api.resource.QuantityValue"
    )
}

fn encode_quantity(message: &str, field: &ProtoField, value: &Value) -> Result<Vec<u8>> {
    let s = value
        .as_str()
        .ok_or_else(|| type_mismatch(message, field, "a quantity string", value))?;
    let mut out = Vec::new();
    if !s.is_empty() {
        wire::encode_tag(1, WireType::LengthDelimited, &mut out);
        wire::encode_length_delimited(s.as_bytes(), &mut out);
    }
    Ok(out)
}

fn decode_quantity_message(field_label: &str, bytes: &[u8]) -> Result<Value> {
    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        if field_number == 1 {
            return Ok(Value::String(
                String::from_utf8_lossy(as_bytes(field_label, &raw)?).into_owned(),
            ));
        }
    }
    Ok(Value::String(String::new()))
}

/// Real upstream's own `JSONSchemaPropsOrArray`/`JSONSchemaPropsOrBool` —
/// the *third* occurrence this codec has needed of the same real
/// pattern `is_time_message`/`is_json_message` already established: a
/// Go type with hand-written `MarshalJSON`/`UnmarshalJSON` that doesn't
/// marshal as its own struct shape at all. `JSONSchemaProps.items` in
/// real JSON is either a single schema object or an array of schemas —
/// written completely unwrapped, never as `{"schema": {...}}` or
/// `{"jSONSchemas": [...]}` — but the vendored proto wraps it as
/// `JSONSchemaPropsOrArray { schema = 1; repeated jSONSchemas = 2; }`.
/// `JSONSchemaProps.additionalProperties` is either a bare `true`/`false`
/// or a schema object — `JSONSchemaPropsOrBool { allows = 1; schema =
/// 2; }` on the wire. Found live, the same way the other two were: a
/// stored `CustomResourceDefinition`'s own `items`/`additionalProperties`
/// silently decoded to an empty object (no `properties`, no `type`,
/// nothing) on every real read, discovered only once
/// `tests/crd_roundtrip.rs`'s own strategic-merge-patch-against-a-CRD
/// test recursed into a list field's `items` schema for real.
fn is_json_schema_props_or_array(message: &str) -> bool {
    matches!(
        message,
        "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaPropsOrArray"
            | "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1beta1.JSONSchemaPropsOrArray"
    )
}

fn is_json_schema_props_or_bool(message: &str) -> bool {
    matches!(
        message,
        "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaPropsOrBool"
            | "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1beta1.JSONSchemaPropsOrBool"
    )
}

/// The real `JSONSchemaProps` message name for a `...OrArray`/`...OrBool`
/// wrapper of the same version — `"...v1.JSONSchemaPropsOrArray"` ->
/// `"...v1.JSONSchemaProps"` — both wrapper messages nest exactly that
/// type, for exactly the version they themselves belong to.
fn json_schema_props_message_for(wrapper_message: &str) -> String {
    let base = wrapper_message
        .rsplit_once('.')
        .map(|(prefix, _)| prefix)
        .unwrap_or(wrapper_message);
    format!("{base}.JSONSchemaProps")
}

fn encode_json_schema_props_or_array(wrapper_message: &str, value: &Value) -> Result<Vec<u8>> {
    let inner = json_schema_props_message_for(wrapper_message);
    let mut out = Vec::new();
    match value {
        Value::Array(items) => {
            for item in items {
                let bytes = encode_message(&inner, item)?;
                wire::encode_tag(2, WireType::LengthDelimited, &mut out);
                wire::encode_length_delimited(&bytes, &mut out);
            }
        }
        _ => {
            let bytes = encode_message(&inner, value)?;
            wire::encode_tag(1, WireType::LengthDelimited, &mut out);
            wire::encode_length_delimited(&bytes, &mut out);
        }
    }
    Ok(out)
}

fn decode_json_schema_props_or_array(wrapper_message: &str, bytes: &[u8]) -> Result<Value> {
    let inner = json_schema_props_message_for(wrapper_message);
    let mut pos = 0;
    let mut schema_bytes: Option<&[u8]> = None;
    let mut list_items = Vec::new();
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        match field_number {
            1 => schema_bytes = Some(as_bytes("JSONSchemaPropsOrArray.schema", &raw)?),
            2 => list_items.push(decode_message(
                &inner,
                as_bytes("JSONSchemaPropsOrArray.jSONSchemas", &raw)?,
            )?),
            _ => {}
        }
    }
    if !list_items.is_empty() {
        return Ok(Value::Array(list_items));
    }
    match schema_bytes {
        Some(b) => decode_message(&inner, b),
        // Real zero value: no `items` schema was ever actually written
        // (both fields absent) -- an empty schema, matching what a
        // never-set `*JSONSchemaProps` field marshals to.
        None => Ok(Value::Object(Map::new())),
    }
}

fn encode_json_schema_props_or_bool(wrapper_message: &str, value: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match value {
        Value::Bool(allows) => {
            // `false` is proto2's own zero value for an optional bool --
            // omitted entirely, matching every other scalar field's own
            // "absent means default" convention this codec already
            // established elsewhere (`encode_time_string`'s own doc
            // comment on the epoch case is the same idiom).
            if *allows {
                wire::encode_tag(1, WireType::Varint, &mut out);
                wire::encode_varint(1, &mut out);
            }
        }
        _ => {
            let inner = json_schema_props_message_for(wrapper_message);
            let bytes = encode_message(&inner, value)?;
            wire::encode_tag(2, WireType::LengthDelimited, &mut out);
            wire::encode_length_delimited(&bytes, &mut out);
        }
    }
    Ok(out)
}

fn decode_json_schema_props_or_bool(wrapper_message: &str, bytes: &[u8]) -> Result<Value> {
    let inner = json_schema_props_message_for(wrapper_message);
    let mut pos = 0;
    let mut allows = false;
    let mut schema_bytes: Option<&[u8]> = None;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        match field_number {
            1 => allows = as_varint("JSONSchemaPropsOrBool.allows", &raw)? != 0,
            2 => schema_bytes = Some(as_bytes("JSONSchemaPropsOrBool.schema", &raw)?),
            _ => {}
        }
    }
    match schema_bytes {
        Some(b) => decode_message(&inner, b),
        None => Ok(Value::Bool(allows)),
    }
}

fn decode_map_entry(message: &str, field: &ProtoField, raw: &RawField) -> Result<Value> {
    let (key_type, value_type) = split_map_type(&field.proto_type)?;
    let entry_bytes = as_bytes(&format!("{message}.{}", field.json_name), raw)?;
    let key_field = ProtoField {
        message: field.message,
        json_name: "key",
        number: 1,
        repeated: false,
        map: false,
        proto_type: key_type,
    };
    let value_field = ProtoField {
        message: field.message,
        json_name: "value",
        number: 2,
        repeated: false,
        map: false,
        proto_type: value_type,
    };
    let mut key: Option<String> = None;
    let mut val: Value = Value::Null;
    let mut pos = 0;
    while pos < entry_bytes.len() {
        let (num, r) = wire::decode_field(entry_bytes, &mut pos)?;
        if num == 1 {
            key = Some(
                decode_one(message, &key_field, &r)?
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
        } else if num == 2 {
            val = decode_one(message, &value_field, &r)?;
        }
    }
    let mut obj = Map::new();
    obj.insert(key.unwrap_or_default(), val);
    Ok(Value::Object(obj))
}

fn as_varint(field: &str, raw: &RawField) -> Result<u64> {
    match raw {
        RawField::Varint(v) => Ok(*v),
        _ => Err(Error::UnexpectedWireShape {
            field: field.to_string(),
        }),
    }
}

fn as_fixed64(field: &str, raw: &RawField) -> Result<f64> {
    match raw {
        RawField::Fixed64(v) => Ok(*v),
        _ => Err(Error::UnexpectedWireShape {
            field: field.to_string(),
        }),
    }
}

// Only the inner `'a` (the wire buffer's own lifetime) is named — the
// returned slice borrows the original bytes, independent of how long the
// `RawField` wrapper value itself is borrowed for.
fn as_bytes<'a>(field: &str, raw: &RawField<'a>) -> Result<&'a [u8]> {
    match raw {
        RawField::LengthDelimited(b) => Ok(b),
        _ => Err(Error::UnexpectedWireShape {
            field: field.to_string(),
        }),
    }
}

fn split_map_type(proto_type: &str) -> Result<(&'static str, &'static str)> {
    let inner = proto_type
        .strip_prefix("map<")
        .and_then(|s| s.strip_suffix('>'))
        .ok_or_else(|| Error::MalformedMapType(proto_type.to_string()))?;
    let (k, v) = inner
        .split_once(',')
        .ok_or_else(|| Error::MalformedMapType(proto_type.to_string()))?;
    // Leaked once per distinct map type at parse time — a small, bounded
    // set (map field variants are rare in the k8s API), and ProtoField's
    // fields are `&'static str` throughout, so this keeps the synthetic
    // key/value ProtoFields built above the same shape as every real one
    // rather than introducing an owned-string variant just for this case.
    let k: &'static str = Box::leak(k.trim().to_string().into_boxed_str());
    let v: &'static str = Box::leak(v.trim().to_string().into_boxed_str());
    Ok((k, v))
}

fn type_mismatch(
    message: &str,
    field: &ProtoField,
    expected: &'static str,
    value: &Value,
) -> Error {
    Error::TypeMismatch {
        message: message.to_string(),
        field: field.json_name.to_string(),
        proto_type: field.proto_type.to_string(),
        expected,
        value: value.clone(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarKind {
    Bool,
    Int32,
    Int64,
    Double,
    String,
    Bytes,
}

impl ScalarKind {
    /// `None` means "not a scalar — a message reference". `map<...>` is
    /// handled separately by callers before this is ever consulted.
    fn of(proto_type: &str) -> Option<ScalarKind> {
        match proto_type {
            "bool" => Some(ScalarKind::Bool),
            "int32" => Some(ScalarKind::Int32),
            "int64" => Some(ScalarKind::Int64),
            "double" => Some(ScalarKind::Double),
            "string" => Some(ScalarKind::String),
            "bytes" => Some(ScalarKind::Bytes),
            _ => None,
        }
    }
}

fn base64_decode(s: &str) -> std::result::Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s)
}

fn base64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}
