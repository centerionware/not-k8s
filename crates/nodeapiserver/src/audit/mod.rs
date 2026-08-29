//! Policy-driven audit pipeline (the four stages) and its backends.
//!
//! `event` — a pure builder for one real `audit.k8s.io/v1` `Event`
//! document, `Metadata` level, `ResponseComplete` stage only — see that
//! module's own doc comment for exactly what's real and what's a named,
//! narrow scope limit (no request/response body logging, one stage only
//! — including a named `watch`-specific inaccuracy). **Wired into
//! `server::listener::handle_with_audit`**, logged one JSON line per
//! request via this crate's own `tracing` output and, when configured,
//! an append-only JSON-lines file sink selected by
//! `NODEAPISERVER_AUDIT_LOG_PATH`. Rotation and webhook delivery remain
//! separate backends.
//!
//! Status: started (Group M — see docs/APISERVER.md). Audit policy
//! (per-rule level selection — every request is unconditionally logged
//! at `Metadata` level today, real upstream's own policy-driven
//! per-rule level selection isn't modeled), file rotation/webhook
//! delivery, and APF (FlowSchema/
//! PriorityLevelConfiguration enforcement) are all separate,
//! not-yet-started work.

pub mod event;
pub mod sink;
