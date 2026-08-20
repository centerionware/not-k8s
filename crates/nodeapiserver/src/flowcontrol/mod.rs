//! API Priority and Fairness: FlowSchema/PriorityLevelConfiguration queueing.
//!
//! `flow_schema` — real `FlowSchema` matching (`matches_flow_schema`/
//! `select_flow_schema`), a faithful port of real upstream's own
//! `pkg/util/flowcontrol/rule.go` + `apihelpers.FlowSchemaSequence` — see
//! that module's own doc comment for exactly what's ported and what
//! isn't. Pure matching primitive only, not yet wired to real storage or
//! the listener, and no concurrency-limiting/queuing exists at all yet
//! (the much larger remaining piece of real APF).
//!
//! Status: started (Group M — see docs/APISERVER.md).

pub mod flow_schema;
