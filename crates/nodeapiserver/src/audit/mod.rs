//! Policy-driven audit pipeline and its backends.
//!
//! `event` — a pure builder for one real `audit.k8s.io/v1` `Event`
//! document at the caller-selected stage and `Metadata` level — see that
//! module's own doc comment for exactly what's real and what's a named,
//! narrow scope limit (no request/response body logging or `Panic` stage).
//! **Wired into
//! `server::listener::handle_with_audit`**, logged one JSON line per
//! request via this crate's own `tracing` output and, when configured,
//! an append-only JSON-lines file sink selected by
//! `NODEAPISERVER_AUDIT_LOG_PATH`. Rotation and webhook delivery remain
//! separate backends. `policy` matches an upstream-shaped
//! `audit.k8s.io/v1` policy in order and supports `None` suppression and
//! `omitStages` for the emitted audit stages. Request and response object
//! capture remains separate work.
//!
//! Status: started (Group M — see docs/APISERVER.md). File
//! rotation/webhook delivery and request/response object capture remain
//! separate work.

pub mod event;
pub mod policy;
pub mod sink;
