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
//! **No concurrency-limiting/queuing exists at all yet** (the much larger
//! remaining piece of real APF — real upstream's own fair-queuing/
//! seat-borrowing algorithm) — every request still runs at full priority,
//! just correctly labeled.
//!
//! Status: in progress (Group M — see docs/APISERVER.md).

pub mod flow_schema;
pub mod resolve;
