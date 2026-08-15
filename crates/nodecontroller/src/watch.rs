//! The Node watch, and the backoff that paces it when it cannot even start.
//!
//! Copied from `nodescheduler::watch`'s `WatchBackoffPolicy`/`watch_nodes`
//! rather than shared via a dependency: it's ~30 lines, and the two crates
//! having no dependency on each other (only both depending on
//! `node-leaderelection`) is a deliberate property worth the small
//! duplication — see CLAUDE.md's crate-isolation reasoning for
//! `nodeproxy`/`nodelet`, same argument applies laterally here.
//!
//! `kube::runtime::watcher` re-lists and self-heals across an *interrupted*
//! stream, but does not pace a watch that cannot **start** — with the
//! apiserver down, every poll returns `WatchStartFailed` immediately, and a
//! loop that merely logs and polls again spins at full CPU. This is what
//! `nodescheduler` found live in its first real run (see that crate's
//! `watch.rs` for the incident); this component starts from the fix rather
//! than rediscovering the bug.

use futures::stream::BoxStream;
use futures::StreamExt;
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::Node;
use kube::runtime::utils::{Backoff, WatchStreamExt};
use kube::runtime::watcher;
use kube::runtime::watcher::Event;
use kube::{Api, Client};

/// Same namespace nodelet's own heartbeat Lease lives in
/// (`crates/nodelet/src/node.rs`'s `LEASE_NS`) — this is upstream's real
/// `NodeLease` mechanism, not a not-k8s invention: a per-node Lease renewed
/// cheaply and frequently (this project's `node-monitor-period=10s`) is what
/// node-lifecycle-controller actually watches for liveness upstream too,
/// not the much heavier full NodeStatus push.
pub const NODE_LEASE_NAMESPACE: &str = "kube-node-lease";

const WATCH_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);
const WATCH_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

fn watch_backoff(consecutive_failures: u32) -> std::time::Duration {
    if consecutive_failures == 0 {
        return std::time::Duration::ZERO;
    }
    let doubled = WATCH_INITIAL_BACKOFF
        .checked_mul(1u32 << (consecutive_failures - 1).min(16))
        .unwrap_or(WATCH_MAX_BACKOFF);
    doubled.min(WATCH_MAX_BACKOFF)
}

#[derive(Default)]
struct WatchBackoffPolicy {
    consecutive_failures: u32,
}

impl Iterator for WatchBackoffPolicy {
    type Item = std::time::Duration;

    fn next(&mut self) -> Option<Self::Item> {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        Some(watch_backoff(self.consecutive_failures))
    }
}

impl Backoff for WatchBackoffPolicy {
    fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

pub fn watch_nodes(client: &Client) -> BoxStream<'static, watcher::Result<Event<Node>>> {
    let api: Api<Node> = Api::all(client.clone());
    watcher(api, watcher::Config::default()).backoff(WatchBackoffPolicy::default()).boxed()
}

pub fn watch_node_leases(client: &Client) -> BoxStream<'static, watcher::Result<Event<Lease>>> {
    let api: Api<Lease> = Api::namespaced(client.clone(), NODE_LEASE_NAMESPACE);
    watcher(api, watcher::Config::default()).backoff(WatchBackoffPolicy::default()).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_failures_means_no_wait() {
        assert_eq!(watch_backoff(0), std::time::Duration::ZERO);
    }

    #[test]
    fn backoff_doubles_up_to_the_ceiling() {
        assert_eq!(watch_backoff(1), std::time::Duration::from_millis(500));
        assert_eq!(watch_backoff(2), std::time::Duration::from_millis(1000));
        assert_eq!(watch_backoff(3), std::time::Duration::from_millis(2000));
    }

    #[test]
    fn backoff_is_capped_and_does_not_overflow_on_many_failures() {
        assert_eq!(watch_backoff(100), WATCH_MAX_BACKOFF);
    }
}
