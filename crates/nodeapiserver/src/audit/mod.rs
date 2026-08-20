//! Policy-driven audit pipeline (the four stages) and its backends.
//!
//! `event` — a pure builder for one real `audit.k8s.io/v1` `Event`
//! document, `Metadata` level, `ResponseComplete` stage only — see that
//! module's own doc comment for exactly what's real and what's a named,
//! narrow scope limit (no request/response body logging, no multi-stage
//! events for long-running requests). **Not yet wired into
//! `server::listener`, and no real audit-log sink (file/webhook) exists
//! at all** — this is the builder primitive only, same "land it, verify
//! it, wire it later" pattern this crate has used repeatedly.
//!
//! Status: started (Group M — see docs/APISERVER.md). Audit policy
//! (per-rule level selection), a real log-file/webhook backend, and APF
//! (FlowSchema/PriorityLevelConfiguration enforcement) are all separate,
//! not-yet-started work.

pub mod event;
