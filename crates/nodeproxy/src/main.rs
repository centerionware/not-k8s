//! nodeproxy — Service (ClusterIP/NodePort) routing for not-k8s.
//!
//! kube-proxy's job, done with nftables and reconcile-on-event instead of
//! iptables and a periodic resync. Pairs with `nodelet` (the node agent) but
//! depends on nothing in it: the two processes share only the apiserver they
//! both watch, and either runs fine without the other.
//!
//! Replaceable on purpose — a node that wants Cilium's eBPF datapath, a real
//! kube-proxy, or no service proxy at all simply doesn't run this binary.
//! `deploy/bootstrap-source.sh --proxy=none` is that path.
//!
//! Needs `nft` and CAP_NET_ADMIN/root. See `svc.rs` for the ruleset itself.
//!
//! Everything below the process boundary lives in `lib.rs`, so the combined
//! `notk8s` binary can run exactly this code path without a second copy of
//! the dependency tree.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    // Runs forever; only returns on a condition that makes the whole process
    // pointless (no usable nft). Exit non-zero so the service manager's
    // restart loop makes that visible instead of leaving a live-looking
    // process that routes nothing.
    if nodeproxy::run().await.is_err() {
        std::process::exit(1);
    }
    Ok(())
}
