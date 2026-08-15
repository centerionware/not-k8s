//! nodecontroller — kube-controller-manager's job for not-k8s.
//!
//! A pure apiserver client: no root, no nftables, no container runtime,
//! just a kubeconfig — same posture as `nodescheduler`. Runs as its own
//! process on purpose, exactly like every other not-k8s component: a
//! cluster that wants the real kube-controller-manager, or none at all,
//! simply doesn't run this binary (`deploy/bootstrap-source.sh
//! --controller-manager=none`, the default, leaves k3s's own bundled one
//! enabled).
//!
//! Everything below the process boundary lives in `lib.rs`, so the combined
//! `notk8s` binary can run exactly this code path without a second copy of
//! the dependency tree.

use anyhow::Result;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    // Runs forever; only returns on a condition that makes the whole process
    // pointless (an unreachable apiserver, an unparseable config, a lost
    // leader-election lease). Returning the error rather than exiting on it
    // is what gets it *printed* — see nodeproxy's main.rs for the incident
    // that established this rule.
    nodecontroller::run().await
}
