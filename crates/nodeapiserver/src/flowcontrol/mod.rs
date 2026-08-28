//! API Priority and Fairness: FlowSchema/PriorityLevelConfiguration queueing.
//!
//! `flow_schema` — real `FlowSchema` matching (`matches_flow_schema`/
//! `select_flow_schema`), a faithful port of real upstream's own
//! `pkg/util/flowcontrol/rule.go` + `apihelpers.FlowSchemaSequence` — see
//! that module's own doc comment for exactly what's ported and what
//! isn't.
//!
//! `resolve` — the storage-backed half: fetches real `FlowSchema`/
//! `PriorityLevelConfiguration` objects and identifies which pair governs
//! a request, wired into `server::listener` to set the real
//! `X-Kubernetes-PF-FlowSchema-UID`/`X-Kubernetes-PF-PriorityLevel-UID`
//! response headers on every request — see that module's own doc comment
//! for exactly what's covered.
//!
//! `limiter` — the request admission gate: bounded FIFO concurrency for
//! ordinary requests, with exempt priority levels and long-running streams
//! excluded from the finite request budget.
//! The full upstream shuffle-sharded per-flow queue and seat-borrowing
//! algorithm remain separate refinements; this gate still enforces finite
//! request and mutating-request budgets.
//!
//! Status: in progress (Group M — see docs/APISERVER.md).

pub mod flow_schema;
pub mod limiter;
pub mod resolve;
