//! Group N: streaming and proxy subresources — exec/attach/port-forward/
//! log spliced through to `nodelet:10250`
//! (`crates/nodelet/src/server/exec.rs`'s proven raw-upgrade-splice
//! pattern), node/service/pod proxy subresources.
//!
//! `pod_log` — real `pods/log` target resolution (`LogLocation`), a
//! faithful port of real upstream's own
//! `pkg/registry/core/pod/strategy.go` + the node connection-info
//! resolution `pkg/kubelet/client/kubelet_client.go` performs. Pure
//! target-resolution only — see that module's own doc comment for
//! exactly what's not yet wired and why (this build has no credential it
//! can present that nodelet's own `TokenReview` authenticator accepts
//! yet).
//!
//! Status: started (Group N — see docs/APISERVER.md).

pub mod pod_log;
