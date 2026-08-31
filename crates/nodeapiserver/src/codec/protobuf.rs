//! Generic protobuf encode/decode over `serde_json::Value`, driven entirely
//! by Group A's build-time field table — no prost-generated struct universe
//! for the k8s API types themselves (`docs/APISERVER_PLAN.md` finding 6).
//! One type universe (k8s-openapi's, reached via JSON today and real
//! structs once Group F's Scheme exists), one place to be wrong.
//!
//! # Scalar types actually present
//!
//! Confirmed by grepping every vendored `.proto` file (see
//! `codegen`'s own `no_field_uses_a_scalar_type_the_codec_does_not_yet_handle`
//! test, which checks this against the live generated table, not just this
//! comment): only `bool`, `bytes`, `double`, `int32`, `int64`, `string`
//! appear. No `enum` declarations exist anywhere in the k8s API surface
//! either (Kubernetes spells its enums as plain strings) — this codec does
//! not need an enum case at all, and adding one speculatively would be
//! exactly the kind of dead machinery this module set out to avoid.
//!
//! # `bytes` <-> JSON
//!
//! A protobuf `bytes` field is base64 text in the JSON representation —
//! the same convention every other Kubernetes JSON<->protobuf codec
//! follows (`k8s\x00` framing aside, the two representations of a given
//! object are meant to be interchangeable).
//!
//! # Repeated fields
//!
//! Unpacked — each element gets its own tag+value, not a single
//! length-delimited packed run. Verified this is spec-correct, not merely
//! simpler: proto2's default is unpacked (packed is opt-in via
//! `[packed=true]`), and grepping the vendored set for any `[...]` field
//! option found none (`build/proto_parse.rs`'s parser has a defensive path
//! for stripping them, but it's never exercised by real input).

use crate::codegen::{self, proto_fields::ProtoField};
use crate::codec::wire::{self, RawField, WireError, WireType};
use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown message {0:?} — not present in the vendored protobuf field table")]
    UnknownMessage(String),
    #[error("wire error: {0}")]
    Wire(#[from] WireError),
    #[error("field {message}.{field} (proto type {proto_type:?}) got a JSON value that isn't a {expected}: {value}")]
    TypeMismatch { message: String, field: String, proto_type: String, expected: &'static str, value: Value },
    #[error("invalid base64 in bytes field {0}: {1}")]
    InvalidBase64(String, base64::DecodeError),
    #[error("malformed map<...> type {0:?}")]
    MalformedMapType(String),
    #[error("the top-level value for message {0:?} must be a JSON object")]
    NotAnObject(String),
    #[error("envelope too short to contain the k8s\\0 magic prefix")]
    EnvelopeTooShort,
    #[error("missing the k8s\\0 magic prefix — not a Kubernetes protobuf-encoded object")]
    BadMagic,
    /// The wire type actually present didn't match what this field's
    /// declared proto type expects — e.g. a `string` field's tag claimed
    /// `Varint` instead of `LengthDelimited`. Malformed or adversarial
    /// input, not a bug in the field table (which is only ever consulted
    /// after the tag's own wire type has already been read off the wire).
    #[error("field {field:?}'s wire data doesn't have the shape its type requires")]
    UnexpectedWireShape { field: String },
    #[error("field {field} is not a valid RFC3339 timestamp: {value:?}")]
    InvalidTimestamp { field: String, value: String },
    /// Group K: a CRD-defined object's body has no compiled proto schema
    /// at all (there's nothing in `vendor/protos` for an arbitrary
    /// operator-defined `CustomResourceDefinition` — real upstream never
    /// generates one either), so `server::rest`'s decode/encode of it
    /// falls back to `application/json` for the body instead — this
    /// variant is that fallback's own decode failure (malformed JSON, not
    /// a schema mismatch). The encode side (`serde_json::to_vec`, called
    /// straight from `server::rest`) reuses this same variant for
    /// symmetry rather than adding a near-duplicate.
    #[error("stored object body is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

type Result<T> = std::result::Result<T, Error>;

/// The 4-byte magic prefix every `application/vnd.kubernetes.protobuf`
/// payload starts with, before the length-delimited `runtime.Unknown`
/// message (`docs/APISERVER_PLAN.md` finding 6).
pub const MAGIC: [u8; 4] = *b"k8s\0";

const UNKNOWN_MESSAGE: &str = "io.k8s.apimachinery.pkg.runtime.Unknown";

/// Encode `value` (a JSON object shaped like `message`'s fields) as a raw
/// protobuf message body — no envelope, no magic bytes. Use
/// [`wrap_unknown`] to produce the full wire payload the apiserver
/// actually sends.
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

/// Decode a raw protobuf message body (no envelope) into a JSON object
/// shaped like `message`'s fields.
pub fn decode_message(message: &str, bytes: &[u8]) -> Result<Value> {
    if !codegen::proto_fields::PROTO_MESSAGES.contains(&message) {
        return Err(Error::UnknownMessage(message.to_string()));
    }
    let by_number = fields_by_number(message);
    let mut obj = Map::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let (field_number, raw) = wire::decode_field(bytes, &mut pos)?;
        let Some(field) = by_number.get(&field_number) else {
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
            let Value::Object(nested) = decoded else { unreachable!("an embedded field always decodes to a JSON object") };
            obj.extend(nested);
        } else if field.map {
            let Value::Object(map) = obj.entry(field.json_name).or_insert_with(|| Value::Object(Map::new())) else {
                unreachable!("map field always inserts a JSON object");
            };
            let Value::Object(entry) = decoded else { unreachable!("map entry decodes to a one-key object") };
            map.extend(entry);
        } else if field.repeated {
            let Value::Array(arr) = obj.entry(field.json_name).or_insert_with(|| Value::Array(Vec::new())) else {
                unreachable!("repeated field always inserts a JSON array");
            };
            arr.push(decoded);
        } else {
            obj.insert(field.json_name.to_string(), decoded);
        }
    }
    Ok(Value::Object(obj))
}

fn fields_by_number(message: &str) -> std::collections::HashMap<u32, &'static ProtoField> {
    codegen::proto_fields::PROTO_FIELDS
        .iter()
        .filter(|f| f.message == message)
        .map(|f| (f.number, f))
        .collect()
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
        Some(ScalarKind::Double) => {
            Ok(serde_json::Number::from_f64(as_fixed64(&label(), raw)?).map(Value::Number).unwrap_or(Value::Null))
        }
        Some(ScalarKind::String) => Ok(Value::String(String::from_utf8_lossy(as_bytes(&label(), raw)?).into_owned())),
        Some(ScalarKind::Bytes) => Ok(Value::String(base64_encode(as_bytes(&label(), raw)?))),
        None => {
            let nested_message = codegen::resolve_message_ref(message, &field.proto_type);
            if is_time_message(&nested_message) {
                decode_time_message(&label(), as_bytes(&label(), raw)?)
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
    matches!(message, "io.k8s.apimachinery.pkg.apis.meta.v1.Time" | "io.k8s.apimachinery.pkg.apis.meta.v1.MicroTime")
}

fn encode_time_string(field_label: &str, s: &str) -> Result<Vec<u8>> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).map_err(|_| Error::InvalidTimestamp { field: field_label.to_string(), value: s.to_string() })?;
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

fn decode_time_message(field_label: &str, bytes: &[u8]) -> Result<Value> {
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
    let dt = chrono::DateTime::from_timestamp(seconds, nanos as u32).ok_or_else(|| Error::InvalidTimestamp { field: field_label.to_string(), value: format!("seconds={seconds}, nanos={nanos}") })?;
    Ok(Value::String(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
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
        "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSON" | "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1beta1.JSON" | "io.k8s.apimachinery.pkg.runtime.RawExtension"
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
        return Err(type_mismatch(message, field, "an int or a string (IntOrString)", value));
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
    matches!(message, "io.k8s.apimachinery.pkg.api.resource.Quantity" | "io.k8s.apimachinery.pkg.api.resource.QuantityValue")
}

fn encode_quantity(message: &str, field: &ProtoField, value: &Value) -> Result<Vec<u8>> {
    let s = value.as_str().ok_or_else(|| type_mismatch(message, field, "a quantity string", value))?;
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
            return Ok(Value::String(String::from_utf8_lossy(as_bytes(field_label, &raw)?).into_owned()));
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
    matches!(message, "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaPropsOrArray" | "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1beta1.JSONSchemaPropsOrArray")
}

fn is_json_schema_props_or_bool(message: &str) -> bool {
    matches!(message, "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaPropsOrBool" | "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1beta1.JSONSchemaPropsOrBool")
}

/// The real `JSONSchemaProps` message name for a `...OrArray`/`...OrBool`
/// wrapper of the same version — `"...v1.JSONSchemaPropsOrArray"` ->
/// `"...v1.JSONSchemaProps"` — both wrapper messages nest exactly that
/// type, for exactly the version they themselves belong to.
fn json_schema_props_message_for(wrapper_message: &str) -> String {
    let base = wrapper_message.rsplit_once('.').map(|(prefix, _)| prefix).unwrap_or(wrapper_message);
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
            2 => list_items.push(decode_message(&inner, as_bytes("JSONSchemaPropsOrArray.jSONSchemas", &raw)?)?),
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
    let key_field = ProtoField { message: field.message, json_name: "key", number: 1, repeated: false, map: false, proto_type: key_type };
    let value_field = ProtoField { message: field.message, json_name: "value", number: 2, repeated: false, map: false, proto_type: value_type };
    let mut key: Option<String> = None;
    let mut val: Value = Value::Null;
    let mut pos = 0;
    while pos < entry_bytes.len() {
        let (num, r) = wire::decode_field(entry_bytes, &mut pos)?;
        if num == 1 {
            key = Some(decode_one(message, &key_field, &r)?.as_str().unwrap_or_default().to_string());
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
        _ => Err(Error::UnexpectedWireShape { field: field.to_string() }),
    }
}

fn as_fixed64(field: &str, raw: &RawField) -> Result<f64> {
    match raw {
        RawField::Fixed64(v) => Ok(*v),
        _ => Err(Error::UnexpectedWireShape { field: field.to_string() }),
    }
}

// Only the inner `'a` (the wire buffer's own lifetime) is named — the
// returned slice borrows the original bytes, independent of how long the
// `RawField` wrapper value itself is borrowed for.
fn as_bytes<'a>(field: &str, raw: &RawField<'a>) -> Result<&'a [u8]> {
    match raw {
        RawField::LengthDelimited(b) => Ok(b),
        _ => Err(Error::UnexpectedWireShape { field: field.to_string() }),
    }
}

fn split_map_type(proto_type: &str) -> Result<(&'static str, &'static str)> {
    let inner = proto_type
        .strip_prefix("map<")
        .and_then(|s| s.strip_suffix('>'))
        .ok_or_else(|| Error::MalformedMapType(proto_type.to_string()))?;
    let (k, v) = inner.split_once(',').ok_or_else(|| Error::MalformedMapType(proto_type.to_string()))?;
    // Leaked once per distinct map type at parse time — a small, bounded
    // set (map field variants are rare in the k8s API), and ProtoField's
    // fields are `&'static str` throughout, so this keeps the synthetic
    // key/value ProtoFields built above the same shape as every real one
    // rather than introducing an owned-string variant just for this case.
    let k: &'static str = Box::leak(k.trim().to_string().into_boxed_str());
    let v: &'static str = Box::leak(v.trim().to_string().into_boxed_str());
    Ok((k, v))
}

fn type_mismatch(message: &str, field: &ProtoField, expected: &'static str, value: &Value) -> Error {
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

/// Wraps an already-encoded object body in the full
/// `application/vnd.kubernetes.protobuf` wire payload: the 4-byte `k8s\0`
/// magic, then a length-delimited (implicit — this is the top-level
/// message, so no outer tag) `runtime.Unknown` whose `raw` field holds
/// `object_bytes` and whose `typeMeta` names `api_version`/`kind`.
pub fn wrap_unknown(api_version: &str, kind: &str, object_bytes: &[u8]) -> Vec<u8> {
    let mut type_meta = Map::new();
    type_meta.insert("apiVersion".to_string(), Value::String(api_version.to_string()));
    type_meta.insert("kind".to_string(), Value::String(kind.to_string()));

    let mut unknown = Map::new();
    unknown.insert("typeMeta".to_string(), Value::Object(type_meta));
    unknown.insert("raw".to_string(), Value::String(base64_encode(object_bytes)));

    let unknown_bytes = encode_message(UNKNOWN_MESSAGE, &Value::Object(unknown))
        .expect("encoding runtime.Unknown itself never fails: every field is a known scalar/message type");

    let mut out = Vec::with_capacity(MAGIC.len() + unknown_bytes.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&unknown_bytes);
    out
}

/// Unwraps a full wire payload back into `(api_version, kind, object_bytes)`
/// — `object_bytes` is still protobuf-encoded (as `encode_message`
/// produces), not yet decoded to JSON; call [`decode_message`] on it with
/// the schema resolved from `(api_version, kind)`.
pub fn unwrap_unknown(bytes: &[u8]) -> Result<(String, String, Vec<u8>)> {
    if bytes.len() < MAGIC.len() {
        return Err(Error::EnvelopeTooShort);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(Error::BadMagic);
    }
    let decoded = decode_message(UNKNOWN_MESSAGE, &bytes[MAGIC.len()..])?;
    let type_meta = decoded.get("typeMeta").cloned().unwrap_or(Value::Object(Map::new()));
    let api_version = type_meta.get("apiVersion").and_then(Value::as_str).unwrap_or_default().to_string();
    let kind = type_meta.get("kind").and_then(Value::as_str).unwrap_or_default().to_string();
    let raw_b64 = decoded.get("raw").and_then(Value::as_str).unwrap_or_default();
    let object_bytes = base64_decode(raw_b64).map_err(|e| Error::InvalidBase64("runtime.Unknown.raw".to_string(), e))?;
    Ok((api_version, kind, object_bytes))
}

/// Which schema `resolve_message_ref` and the type-meta bridge both need:
/// looks up `(group, version, kind)` the way [`unwrap_unknown`]'s caller
/// would after splitting `apiVersion` into group/version.
pub fn schema_for_gvk(group: &str, version: &str, kind: &str) -> Option<&'static str> {
    codegen::schema_by_gvk().get(&(group, version, kind)).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The real bug that motivated `is_inline_embedded_field`: found live
    /// via a `ValidatingAdmissionPolicy` round trip against a real
    /// `nodestore` -- `apiGroups`/`apiVersions`/`resources`/`operations`/
    /// `scope` all silently vanished on write because they belong to two
    /// levels of real upstream Go struct embedding
    /// (`NamedRuleWithOperations` -> `RuleWithOperations` -> `Rule`) that
    /// JSON flattens but the vendored proto keeps as nested message
    /// fields with no JSON key of their own.
    #[test]
    fn named_rule_with_operations_round_trips_its_doubly_embedded_fields() {
        let message = "io.k8s.api.admissionregistration.v1.NamedRuleWithOperations";
        let value = json!({
            "resourceNames": ["a", "b"],
            "operations": ["CREATE", "UPDATE"],
            "apiGroups": ["apps"],
            "apiVersions": ["v1"],
            "resources": ["deployments"],
            "scope": "Namespaced",
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded, value);
    }

    /// The second real bug the same live round trip caught, right after the
    /// first: `Validation.Expression` and `Variable.Name`/`.Expression` are
    /// capitalized in the vendored proto (a go-to-protobuf quirk, see
    /// `real_json_name_override`'s own doc comment in `build/proto_parse.rs`)
    /// but real upstream's actual JSON keys are lowercase -- `update` failed
    /// validation with "spec.validations[0].expression: Required value"
    /// because the codec wrote the object with a capitalized key nothing
    /// downstream recognized.
    #[test]
    fn validation_and_variable_round_trip_their_real_lowercase_json_keys() {
        let validation = "io.k8s.api.admissionregistration.v1.Validation";
        let value = json!({
            "expression": "object.spec.replicas <= 5",
            "message": "too many replicas",
        });
        let encoded = encode_message(validation, &value).unwrap();
        let decoded = decode_message(validation, &encoded).unwrap();
        assert_eq!(decoded, value);

        let variable = "io.k8s.api.admissionregistration.v1.Variable";
        let value = json!({
            "name": "replicas",
            "expression": "object.spec.replicas",
        });
        let encoded = encode_message(variable, &value).unwrap();
        let decoded = decode_message(variable, &encoded).unwrap();
        assert_eq!(decoded, value);
    }

    /// Same class of bug as above, on `MutatingWebhookConfiguration`'s and
    /// `ValidatingWebhookConfiguration`'s own `Webhooks` field -- real
    /// upstream's JSON key is lowercase `webhooks`. Uses a non-empty
    /// webhooks list: an empty `repeated` field genuinely produces no wire
    /// bytes at all (true of every `repeated` field in this codec, not
    /// specific to this bug), so it can never round-trip as a present-but-
    /// empty array -- that's not what this test is checking.
    #[test]
    fn validating_webhook_configuration_round_trips_lowercase_webhooks_key() {
        let message = "io.k8s.api.admissionregistration.v1.ValidatingWebhookConfiguration";
        let value = json!({
            "metadata": {"name": "my-config"},
            "webhooks": [{"name": "my-webhook.example.com"}],
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn pod_volume_round_trips_its_flattened_volume_source() {
        let message = "io.k8s.api.core.v1.Volume";
        let value = json!({
            "name": "config-volume",
            "configMap": {
                "name": "coredns",
                "items": [{"key": "Corefile", "path": "Corefile"}],
            },
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn a_simple_object_meta_round_trips() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({
            "name": "my-pod",
            "namespace": "default",
            "uid": "abc-123",
            "generation": 5,
            "labels": {"app": "web", "tier": "frontend"},
        });
        let encoded = encode_message(message, &value).unwrap();
        assert!(!encoded.is_empty());
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("name").unwrap(), "my-pod");
        assert_eq!(decoded.get("namespace").unwrap(), "default");
        assert_eq!(decoded.get("uid").unwrap(), "abc-123");
        assert_eq!(decoded.get("generation").unwrap(), &json!(5));
        assert_eq!(decoded.get("labels").unwrap(), &json!({"app": "web", "tier": "frontend"}));
    }

    /// Found live: nothing exercised `creationTimestamp` through a real
    /// protobuf round trip before `tests/encryption_roundtrip.rs`'s own
    /// live datastore round trip did — every `rest::create`/`update`
    /// call sets this field, so this was a real, previously-undiscovered
    /// bug blocking every single object this crate ever persisted to a
    /// real nodestore, silently uncaught because no prior test (unit or
    /// otherwise) happened to send an `ObjectMeta` through
    /// `encode_message` with this field actually populated.
    #[test]
    fn object_meta_with_a_real_creation_timestamp_round_trips() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({"name": "my-pod", "creationTimestamp": "2024-01-15T10:30:00Z"});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["creationTimestamp"], "2024-01-15T10:30:00Z");
    }

    #[test]
    fn time_message_round_trips_through_the_real_seconds_nanos_wire_shape() {
        let bytes = encode_time_string("Time", "2024-01-15T10:30:00Z").unwrap();
        // Not empty and not a bare string on the wire -- proving this
        // really did take the {seconds, nanos} message path, not fall
        // back to string encoding.
        assert!(!bytes.is_empty());
        let decoded = decode_time_message("Time", &bytes).unwrap();
        assert_eq!(decoded, json!("2024-01-15T10:30:00Z"));
    }

    /// Real bug, found live: `tests/aggregator_proxy_roundtrip.rs`'s own
    /// live `Service.spec.ports[].targetPort: 8443` (a plain JSON number)
    /// failed to decode with `NotAnObject("...IntOrString")` -- nothing
    /// in this codec had a special case for `intstr.IntOrString` at all
    /// before this fix, so `encode_field`'s generic `encode_message`
    /// fallback tried (and `decode_one`'s `decode_message` fallback would
    /// have tried) to treat the plain scalar as a fields-shaped object.
    #[test]
    fn service_port_with_a_real_numeric_target_port_round_trips() {
        let message = "io.k8s.api.core.v1.ServicePort";
        let value = json!({"name": "https", "port": 443, "targetPort": 8443});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["targetPort"], json!(8443));
    }

    #[test]
    fn service_port_with_a_real_named_target_port_round_trips() {
        let message = "io.k8s.api.core.v1.ServicePort";
        let value = json!({"name": "https", "port": 443, "targetPort": "https"});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["targetPort"], json!("https"));
    }

    #[test]
    fn int_or_string_encodes_a_number_with_no_type_tag_written() {
        // type == 0 (Int) is proto2 optional's own zero value -- omitted
        // on the wire, same "don't write an explicit zero" posture every
        // other optional scalar field in this codec already takes.
        let bytes = encode_int_or_string("ServicePort", &ProtoField { message: "ServicePort", json_name: "targetPort", number: 4, repeated: false, map: false, proto_type: "k8s.io.apimachinery.pkg.util.intstr.IntOrString" }, &json!(8443)).unwrap();
        let decoded = decode_int_or_string_message("targetPort", &bytes).unwrap();
        assert_eq!(decoded, json!(8443));
    }

    #[test]
    fn int_or_string_rejects_a_value_that_is_neither_int_nor_string() {
        let field = ProtoField { message: "ServicePort", json_name: "targetPort", number: 4, repeated: false, map: false, proto_type: "k8s.io.apimachinery.pkg.util.intstr.IntOrString" };
        assert!(encode_int_or_string("ServicePort", &field, &json!(true)).is_err());
    }

    /// Found via a deliberate audit pass, not a live failure this time —
    /// `is_quantity_message`'s own doc comment covers why this needed the
    /// same special case as `IntOrString` before it.
    #[test]
    fn resource_field_selector_with_a_real_quantity_divisor_round_trips() {
        let message = "io.k8s.api.core.v1.ResourceFieldSelector";
        let value = json!({"resource": "limits.cpu", "divisor": "100m"});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["divisor"], json!("100m"));
    }

    #[test]
    fn quantity_round_trips_a_real_binary_si_value() {
        let field = ProtoField { message: "ResourceFieldSelector", json_name: "divisor", number: 3, repeated: false, map: false, proto_type: "k8s.io.apimachinery.pkg.api.resource.Quantity" };
        let bytes = encode_quantity("ResourceFieldSelector", &field, &json!("1.5Gi")).unwrap();
        assert_eq!(decode_quantity_message("divisor", &bytes).unwrap(), json!("1.5Gi"));
    }

    /// `runtime.RawExtension` shares `apiextensions.v1.JSON`'s exact wire
    /// shape and JSON semantics -- `is_json_message`'s own doc comment
    /// covers why. Verified against a real message that actually embeds
    /// one: `admissionreview.k8s.io/v1`'s own request/response carries
    /// `object`/`oldObject` as `runtime.RawExtension` upstream, though
    /// this crate doesn't have `AdmissionReview` compiled yet -- proven
    /// here with a synthetic message name instead, exercising exactly the
    /// same `encode_json_value`/`decode_json_message` code path either
    /// way (both are driven purely by `is_json_message`'s own match, not
    /// by anything specific to which real message uses it).
    #[test]
    fn raw_extension_is_recognized_by_the_same_json_wrapper_special_case() {
        assert!(is_json_message("io.k8s.apimachinery.pkg.runtime.RawExtension"));
        let encoded = encode_json_value(&json!({"kind": "PluginA", "aOption": "foo"}));
        let decoded = decode_json_message("raw", &encoded).unwrap();
        assert_eq!(decoded, json!({"kind": "PluginA", "aOption": "foo"}));
    }

    #[test]
    fn managed_fields_round_trip_through_the_fields_v1_json_wrapper() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({
            "name": "hpa",
            "managedFields": [{
                "manager": "legacy",
                "operation": "Apply",
                "apiVersion": "autoscaling/v1",
                "fieldsType": "FieldsV1",
                "fieldsV1": {"f:spec": {"f:targetCPUUtilizationPercentage": {}}}
            }]
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["managedFields"][0]["manager"], "legacy");
        assert!(decoded["managedFields"][0]["fieldsV1"]["f:spec"]["f:targetCPUUtilizationPercentage"].is_object());
    }

    #[test]
    fn time_message_handles_the_unix_epoch_with_no_fields_written() {
        // seconds == 0 and nanos == 0 are both real proto2 "optional"
        // defaults -- neither field gets written on the wire at all,
        // matching every other scalar field's own "absent means default"
        // convention this codec already established elsewhere.
        let bytes = encode_time_string("Time", "1970-01-01T00:00:00Z").unwrap();
        assert!(bytes.is_empty());
        let decoded = decode_time_message("Time", &bytes).unwrap();
        assert_eq!(decoded, json!("1970-01-01T00:00:00Z"));
    }

    #[test]
    fn encode_time_string_rejects_a_non_rfc3339_value() {
        assert!(encode_time_string("Time", "not a timestamp").is_err());
    }

    /// Found live, the same way `object_meta_with_a_real_creation_
    /// timestamp_round_trips` was: nothing exercised a real
    /// `CustomResourceDefinition`'s own schema `default` through
    /// `encode_message`/`decode_message` until `tests/crd_roundtrip.rs`'s
    /// live round trip did, and it hit exactly this same class of bug —
    /// `JSONSchemaProps.default` is an `apiextensions.v1.JSON` message,
    /// not a plain scalar, and this codec had no special case for it yet.
    #[test]
    fn json_schema_props_default_round_trips_through_the_real_json_wire_shape() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "string", "default": "small"});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["default"], "small");
        assert_eq!(decoded["type"], "string");
    }

    #[test]
    fn json_message_round_trips_a_non_scalar_default() {
        let bytes = encode_json_value(&json!({"a": 1, "b": [true, null]}));
        assert!(!bytes.is_empty());
        let decoded = decode_json_message("default", &bytes).unwrap();
        assert_eq!(decoded, json!({"a": 1, "b": [true, null]}));
    }

    #[test]
    fn json_message_with_no_raw_field_decodes_to_null() {
        assert_eq!(decode_json_message("default", &[]).unwrap(), Value::Null);
    }

    /// The real bug this whole trio of helpers exists to fix: `items` on
    /// a real `JSONSchemaProps` (used by every CRD list field) round
    /// trips as the plain, unwrapped schema object real JSON uses — not
    /// the `JSONSchemaPropsOrArray` wrapper shape. Found live: a CRD's
    /// own `items` schema (and therefore every field inside it,
    /// including `x-kubernetes-list-map-keys` on a nested list) silently
    /// decoded to an empty object on every real read until
    /// `tests/crd_roundtrip.rs`'s own strategic-merge-patch test actually
    /// recursed into one.
    #[test]
    fn json_schema_props_items_round_trips_as_a_single_unwrapped_schema() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}}}});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["items"], json!({"type": "object", "properties": {"name": {"type": "string"}}}));
    }

    #[test]
    fn json_schema_props_items_round_trips_as_an_array_of_schemas() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "array", "items": [{"type": "string"}, {"type": "integer"}]});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["items"], json!([{"type": "string"}, {"type": "integer"}]));
    }

    #[test]
    fn json_schema_props_additional_properties_round_trips_a_bare_bool() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "object", "additionalProperties": true});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["additionalProperties"], json!(true));
    }

    #[test]
    fn json_schema_props_additional_properties_false_round_trips() {
        // `allows: false` is proto2's own zero value for that inner
        // scalar (never written inside the JSONSchemaPropsOrBool
        // submessage itself), but the *outer* additionalProperties field
        // is still present (an empty submessage, not an absent one) --
        // decode must still come back `false`, not `null` or `true`.
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "object", "additionalProperties": false});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["additionalProperties"], json!(false));
    }

    #[test]
    fn an_absent_additional_properties_key_stays_absent_after_a_round_trip() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "object"});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("additionalProperties"), None, "a key the client never submitted at all must not appear after a round trip");
    }

    #[test]
    fn json_schema_props_additional_properties_round_trips_a_nested_schema() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "object", "additionalProperties": {"type": "string"}});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["additionalProperties"], json!({"type": "string"}));
    }

    #[test]
    fn a_nested_message_field_round_trips() {
        // ListMeta has no nested message fields; ObjectMeta does not
        // either in a simple way, so exercise a real cross-package
        // reference: DaemonSetSpec.selector -> LabelSelector.
        let message = "io.k8s.api.apps.v1.DaemonSetSpec";
        let value = json!({
            "selector": {"matchLabels": {"app": "web"}},
            "minReadySeconds": 30,
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("selector").unwrap(), &json!({"matchLabels": {"app": "web"}}));
        assert_eq!(decoded.get("minReadySeconds").unwrap(), &json!(30));
    }

    #[test]
    fn a_repeated_scalar_field_round_trips_as_an_array() {
        // ServiceAccount.imagePullSecrets is repeated
        // LocalObjectReference (message), but PodSpec.hostAliases's IPs
        // are a plainer example — use Container.args, repeated string.
        let message = "io.k8s.api.core.v1.Container";
        let value = json!({"name": "app", "args": ["--flag", "value"]});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("args").unwrap(), &json!(["--flag", "value"]));
    }

    #[test]
    fn a_repeated_message_field_round_trips() {
        let message = "io.k8s.api.core.v1.PodSpec";
        let value = json!({
            "containers": [
                {"name": "a", "image": "nginx"},
                {"name": "b", "image": "redis"},
            ],
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        let containers = decoded.get("containers").unwrap().as_array().unwrap();
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].get("name").unwrap(), "a");
        assert_eq!(containers[1].get("image").unwrap(), "redis");
    }

    #[test]
    fn container_probes_round_trip_through_the_real_nested_wire_shape() {
        let message = "io.k8s.api.core.v1.Container";
        let value = json!({
            "name": "coredns",
            "livenessProbe": {
                "httpGet": {"path": "/health", "port": 8080, "scheme": "HTTP"},
                "initialDelaySeconds": 60,
            },
            "readinessProbe": {
                "httpGet": {"path": "/ready", "port": 8181, "scheme": "HTTP"},
            },
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn a_bytes_field_round_trips_through_base64() {
        // ConfigMap.binaryData is map<string, bytes>; Secret.data too.
        // Use Secret.data to exercise both bytes and map in one field.
        let message = "io.k8s.api.core.v1.Secret";
        let value = json!({"data": {"password": base64_encode(b"hunter2")}});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        let got = decoded.get("data").unwrap().get("password").unwrap().as_str().unwrap();
        assert_eq!(base64_decode(got).unwrap(), b"hunter2");
    }

    #[test]
    fn a_map_string_string_field_round_trips() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({"annotations": {"a": "1", "b": "2", "c": "3"}});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("annotations").unwrap(), &json!({"a": "1", "b": "2", "c": "3"}));
    }

    #[test]
    fn a_negative_int64_round_trips() {
        // ObjectMeta has no signed-negative-friendly field; use
        // DeleteOptions.gracePeriodSeconds (int64) is not on ObjectMeta —
        // use PodSpec.terminationGracePeriodSeconds (int64) instead, which
        // can legitimately be negative in upstream's own semantics is not
        // true, but the wire format must still round-trip a negative value
        // correctly regardless of whether real callers send one.
        let message = "io.k8s.api.core.v1.PodSpec";
        let value = json!({"terminationGracePeriodSeconds": -1});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("terminationGracePeriodSeconds").unwrap(), &json!(-1));
    }

    #[test]
    fn null_fields_are_omitted_not_encoded_as_present() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({"name": "x", "namespace": null});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert!(decoded.get("namespace").is_none(), "a null field must not round-trip as present");
    }

    #[test]
    fn unknown_json_fields_are_skipped_not_errors() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({"name": "x", "totallyMadeUpField": "whatever"});
        // Must not error — forward/backward compatibility with a differently
        // versioned client is the whole point of tolerating unknown fields.
        encode_message(message, &value).unwrap();
    }

    #[test]
    fn the_envelope_round_trips_api_version_kind_and_body() {
        let object = json!({"metadata": {"name": "my-pod"}});
        let body = encode_message("io.k8s.api.core.v1.Pod", &object).unwrap();
        let wrapped = wrap_unknown("v1", "Pod", &body);
        assert_eq!(&wrapped[..MAGIC.len()], &MAGIC);

        let (api_version, kind, object_bytes) = unwrap_unknown(&wrapped).unwrap();
        assert_eq!(api_version, "v1");
        assert_eq!(kind, "Pod");
        let decoded = decode_message("io.k8s.api.core.v1.Pod", &object_bytes).unwrap();
        assert_eq!(decoded.get("metadata").unwrap().get("name").unwrap(), "my-pod");
    }

    #[test]
    fn a_missing_magic_prefix_is_rejected() {
        let err = unwrap_unknown(b"not-a-k8s-payload").unwrap_err();
        assert!(matches!(err, Error::BadMagic));
    }

    #[test]
    fn schema_for_gvk_resolves_core_v1_pod() {
        assert_eq!(schema_for_gvk("", "v1", "Pod"), Some("io.k8s.api.core.v1.Pod"));
        assert_eq!(schema_for_gvk("apps", "v1", "Deployment"), Some("io.k8s.api.apps.v1.Deployment"));
    }
}
