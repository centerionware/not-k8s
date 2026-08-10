//! [`Consensus`] backed by raft.
//!
//! Thin by design: the driver does the work, and this is the adapter that
//! lets the gRPC layer keep proposing commands without knowing whether a
//! quorum was involved.
//!
//! # The handle arrives after construction, not before
//!
//! There is a genuine circularity to resolve. The driver applies committed
//! entries, so it needs the [`Node`]; the `Node` proposes through consensus,
//! so it needs this. Rather than make one of them optional forever, the
//! handle is installed exactly once, immediately after the driver starts, and
//! every method here treats its absence as "the datastore is still coming up"
//! — which, for the few milliseconds it is true, is exactly right.

use crate::command::Command;
use crate::consensus::{Consensus, Submitted};
use crate::error::{Error, Result};
use crate::replication::driver::RaftHandle;
use async_trait::async_trait;
use std::sync::OnceLock;

pub struct RaftConsensus {
    handle: OnceLock<RaftHandle>,
    member_id: u64,
    cluster_id: u64,
}

impl RaftConsensus {
    pub fn new(member_id: u64, cluster_id: u64) -> RaftConsensus {
        RaftConsensus { handle: OnceLock::new(), member_id, cluster_id }
    }

    /// Install the driver's handle. Called once, by `serve`, right after the
    /// driver starts.
    pub fn attach(&self, handle: RaftHandle) {
        if self.handle.set(handle).is_err() {
            // Two drivers for one node would each apply every entry.
            panic!("the raft driver was attached twice");
        }
    }

    fn handle(&self) -> Result<&RaftHandle> {
        self.handle.get().ok_or_else(|| {
            Error::Unavailable("the datastore is still starting up".to_string())
        })
    }
}

#[async_trait]
impl Consensus for RaftConsensus {
    async fn submit(&self, cmd: &Command) -> Result<Submitted> {
        // Applied, not ApplyLocally: the driver applies committed entries on
        // every member, this one included. Applying again here would apply
        // twice on whichever member proposed.
        let applied = self.handle()?.propose(cmd).await?;
        Ok(Submitted::Applied(applied))
    }

    fn is_leader(&self) -> bool {
        self.handle.get().map(|h| h.is_leader()).unwrap_or(false)
    }

    fn term(&self) -> u64 {
        self.handle.get().map(|h| h.term()).unwrap_or(0)
    }

    fn member_id(&self) -> u64 {
        self.member_id
    }

    fn cluster_id(&self) -> u64 {
        self.cluster_id
    }

    fn leader_id(&self) -> Option<u64> {
        self.handle.get().and_then(|h| h.leader_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_node_whose_driver_has_not_started_is_unavailable_not_a_leader() {
        // The window is small but real, and getting it backwards would have a
        // starting member accept writes it cannot replicate.
        let c = RaftConsensus::new(1, 1);
        assert!(!c.is_leader(), "must not claim leadership before the driver exists");
        assert_eq!(c.leader_id(), None);
        assert_eq!(c.term(), 0);

        let err = c
            .submit(&Command::Compact { revision: 1 })
            .await
            .expect_err("a write before startup must fail");
        assert!(matches!(err, Error::Unavailable(_)));
    }

    #[test]
    fn identity_is_available_before_the_driver_is() {
        // Response headers carry these from the first request, including the
        // ones that fail because the driver has not started.
        let c = RaftConsensus::new(7, 9);
        assert_eq!(c.member_id(), 7);
        assert_eq!(c.cluster_id(), 9);
    }
}
