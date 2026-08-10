//! Bringing a clustered member up, and keeping the address book true.
//!
//! # Who publishes the address book
//!
//! Only the leader can propose, so only the leader can write the address book
//! — which means a follower cannot announce its own client URL. The rule that
//! falls out of that, and the one this module implements:
//!
//!   * every member's **peer** URL comes from configuration, which every
//!     member already has, and the leader publishes all of them;
//!   * a member's **client** URL is published by that member *when it becomes
//!     leader*.
//!
//! That is enough, because the only client URL anyone needs is the leader's:
//! forwarding goes follower → leader and never the other way. A follower's
//! client URL being unknown cluster-wide is not a gap, it is unused
//! information.
//!
//! # Why this is re-done on every leadership change
//!
//! A new leader may be publishing its own client URL for the first time, and
//! a member that restarted with a changed address needs the book corrected.
//! Re-proposing an unchanged entry is a no-op at the state machine, so doing
//! it unconditionally is cheaper than working out whether it is needed.

use crate::command::{Command, Member};
use crate::config::Config;
use crate::consensus::Node;
use crate::replication::driver::RaftHandle;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Watch for this member becoming leader and publish the address book when it
/// does.
pub async fn publish_address_book(handle: RaftHandle, node: Arc<Node>, cfg: Config) {
    let mut was_leader = false;
    let mut ticker = tokio::time::interval(Duration::from_millis(250));

    loop {
        ticker.tick().await;
        let is_leader = handle.is_leader();
        if !is_leader {
            was_leader = false;
            continue;
        }
        if was_leader {
            continue;
        }

        info!("this member is now the leader; publishing the cluster address book");
        for (id, peer_url) in &cfg.initial_cluster {
            let member = Member {
                id: *id,
                peer_url: peer_url.clone(),
                // Only this member's own client URL is knowable here. Another
                // member's stays empty until it leads and publishes its own —
                // and nothing needs it before then.
                client_url: if *id == cfg.member_id {
                    cfg.advertise_client_url.clone()
                } else {
                    existing_client_url(&node, *id)
                },
                name: format!("member-{id}"),
                is_learner: false,
            };
            match handle.propose(&Command::SetMember(member)).await {
                Ok(_) => debug!(member = id, "published address book entry"),
                Err(e) => {
                    // Losing leadership mid-publish is ordinary; the next
                    // leader will publish its own.
                    warn!(member = id, error = %e, "could not publish an address book entry");
                    break;
                }
            }
        }
        was_leader = true;
    }
}

/// Keep whatever client URL the book already holds for a member, rather than
/// blanking it. A leader that republishes the book must not erase a client
/// URL a previous leader legitimately recorded.
fn existing_client_url(node: &Arc<Node>, id: u64) -> String {
    node.read(|s| s.member(id))
        .ok()
        .flatten()
        .map(|m| m.client_url)
        .unwrap_or_default()
}

/// A single-member cluster has nobody to campaign against, so it would sit
/// out a full election timeout before electing itself on every start. Nudging
/// it makes startup immediate.
///
/// Only for a genuinely single-member cluster: forcing an election in a real
/// cluster would depose a healthy leader for no reason.
pub async fn campaign_if_alone(handle: RaftHandle, cfg: Config) {
    if cfg.initial_cluster.len() != 1 {
        return;
    }
    // One tick of slack so the driver has stepped at least once.
    tokio::time::sleep(Duration::from_millis(200)).await;
    if handle.is_leader() {
        return;
    }
    match handle.campaign().await {
        Ok(()) => info!("single-member cluster: elected self without waiting out an election timeout"),
        Err(e) => warn!(error = %e, "could not self-elect"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::SingleNode;
    use crate::store::Store;
    use std::path::Path;

    fn node() -> Arc<Node> {
        Node::new(
            Store::open(Path::new(":memory:")).unwrap(),
            Arc::new(SingleNode::new(1, 1)),
            16,
        )
    }

    #[test]
    fn an_unknown_member_has_no_remembered_client_url() {
        assert_eq!(existing_client_url(&node(), 42), "");
    }

    #[tokio::test]
    async fn republishing_does_not_erase_a_client_url_another_leader_recorded() {
        // The bug this guards: leader B republishing the book would otherwise
        // blank A's client URL, and a follower forwarding to A would then have
        // nowhere to send.
        let node = node();
        node.propose(Command::SetMember(Member {
            id: 2,
            peer_url: "http://p2".into(),
            client_url: "http://c2".into(),
            name: "m2".into(),
            is_learner: false,
        }))
        .await
        .unwrap();

        assert_eq!(existing_client_url(&node, 2), "http://c2");
    }
}
