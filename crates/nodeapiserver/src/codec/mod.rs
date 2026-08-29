//! Group B: wire formats and content negotiation.
//!
//! `protobuf`/`wire` — the `application/vnd.kubernetes.protobuf` codec,
//! generic over any message the Group A field table knows about, plus the
//! `k8s\0` + `runtime.Unknown` envelope (finding 6).
//! `json`/`yaml` — thin named wrappers so `negotiation`'s dispatch has one
//! function per format regardless of which crate implements it.
//! `negotiation` — `Accept`/`Content-Type` header parsing, including the
//! `as=Table;g=...;v=...` server-side-printing parameters `kubectl get`
//! sends.
//! `table` — the `Table` conversion itself, negotiated via those
//! parameters: the generic default converter only (real per-type printers
//! like Pod's `READY`/`STATUS` columns are separate, unstarted work — see
//! the module's own doc for exactly what's covered).
//!
//! `partial_metadata` serves the standard `meta.k8s.io/v1`
//! `PartialObjectMetadata` and `PartialObjectMetadataList` representations
//! for negotiated `GET`, `LIST`, and `WATCH` responses.

pub mod wire;
pub mod protobuf;
pub mod json;
pub mod yaml;
pub mod negotiation;
pub mod partial_metadata;
pub mod table;
