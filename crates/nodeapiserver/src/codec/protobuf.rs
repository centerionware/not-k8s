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
include!("protobuf/part-1.rs");
include!("protobuf/part-2.rs");
include!("protobuf/part-3.rs");
include!("protobuf/part-4.rs");
include!("protobuf/part-5.rs");
include!("protobuf/part-6.rs");
include!("protobuf/part-7.rs");
include!("protobuf/part-8.rs");
include!("protobuf/part-9.rs");
include!("protobuf/part-10.rs");
include!("protobuf/part-11.rs");
include!("protobuf/part-12.rs");
include!("protobuf/part-13.rs");
include!("protobuf/part-14.rs");
include!("protobuf/part-15.rs");
include!("protobuf/part-16.rs");
include!("protobuf/part-17.rs");
include!("protobuf/part-18.rs");
include!("protobuf/part-19.rs");
include!("protobuf/part-20.rs");
include!("protobuf/part-21.rs");
include!("protobuf/part-22.rs");
include!("protobuf/part-23.rs");
include!("protobuf/part-24.rs");
include!("protobuf/part-25.rs");
include!("protobuf/part-26.rs");
include!("protobuf/part-27.rs");
include!("protobuf/part-28.rs");
include!("protobuf/part-29.rs");
include!("protobuf/part-30.rs");
include!("protobuf/part-31.rs");
include!("protobuf/part-32.rs");
include!("protobuf/part-33.rs");
include!("protobuf/part-34.rs");
include!("protobuf/part-35.rs");
include!("protobuf/part-36.rs");
include!("protobuf/part-37.rs");
