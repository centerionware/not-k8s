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
//!
//! Not yet done: `Table` server-side printing itself (only its negotiation
//! parameters are parsed so far) and `PartialObjectMetadata`.

pub mod wire;
pub mod protobuf;
pub mod json;
pub mod yaml;
pub mod negotiation;
