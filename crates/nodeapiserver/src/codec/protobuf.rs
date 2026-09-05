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

use crate::codec::wire::{self, RawField, WireError, WireType};
use crate::codegen::{self, proto_fields::ProtoField};
use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown message {0:?} — not present in the vendored protobuf field table")]
    UnknownMessage(String),
    #[error("wire error: {0}")]
    Wire(#[from] WireError),
    #[error(
        "field {message}.{field} (proto type {proto_type:?}) got a JSON value that isn't a {expected}: {value}"
    )]
    TypeMismatch {
        message: String,
        field: String,
        proto_type: String,
        expected: &'static str,
        value: Value,
    },
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

include!("protobuf_encode.rs");
include!("protobuf_decode.rs");
include!("protobuf_envelope.rs");
include!("protobuf_tests.rs");
