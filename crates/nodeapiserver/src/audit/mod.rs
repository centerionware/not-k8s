//! Policy-driven audit pipeline and its backends.
//!
//! `event` — a pure builder for one real `audit.k8s.io/v1` `Event`
//! document at the caller-selected stage and policy-selected level — see that
//! module's own doc comment for exactly what's real and what's a named,
//! narrow scope limit (bounded JSON/YAML object capture and no `Panic` stage).
//! **Wired into
//! `server::listener::handle_with_audit`**, logged one JSON line per
//! request via this crate's own `tracing` output and, when configured,
//! an append-only JSON-lines file sink selected by
//! `NODEAPISERVER_AUDIT_LOG_PATH`. The same sink can also enqueue bounded
//! `EventList` batches for the asynchronous webhook backend selected by
//! `NODEAPISERVER_AUDIT_WEBHOOK_URL`. `policy` matches an upstream-shaped
//! `audit.k8s.io/v1` policy in order and supports `None` suppression and
//! `omitStages` for the emitted audit stages. Request and response object
//! capture is bounded and supports decoded JSON/YAML bodies; protobuf bodies
//! and the `Panic` stage remain out of scope.
//!
//! Status: started (Group M — see docs/APISERVER.md). Request/response object
//! capture is integrated; the Panic stage remains separate work.

pub mod event;
pub mod policy;
pub mod sink;
pub mod webhook;
