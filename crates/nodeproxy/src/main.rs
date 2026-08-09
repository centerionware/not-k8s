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

use anyhow::{Context, Result};
use tracing::error;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod config;
mod svc;

#[tokio::main]
async fn main() -> Result<()> {
    // rustls 0.23 stopped silently picking a default CryptoProvider, and
    // kube::Client::try_default() panics rather than erroring without one.
    // Same call, same reason, as nodelet's own main().
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("installing default rustls CryptoProvider (should only fail if called twice)");

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    let cfg = config::Config::from_env().context("loading configuration")?;

    let client = kube::Client::try_default()
        .await
        .context("building kube client (is KUBECONFIG set and the apiserver reachable?)")?;

    // Runs forever; only returns on a condition that makes the whole process
    // pointless (no usable nft). Exit non-zero so the service manager's
    // restart loop makes that visible instead of leaving a live-looking
    // process that routes nothing.
    if let Err(e) = svc::ServiceProxy::new(client, cfg.ip_family, cfg.lb_method).run().await {
        error!(error = ?e, "service proxy stopped");
        std::process::exit(1);
    }
    Ok(())
}
