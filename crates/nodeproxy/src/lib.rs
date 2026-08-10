//! nodeproxy — Service (ClusterIP/NodePort) routing for not-k8s.
//!
//! kube-proxy's job, done with nftables and reconcile-on-event instead of
//! iptables and a periodic resync. See `main.rs` for the standalone binary
//! and `svc.rs` for the ruleset itself.
//!
//! This is a library so the same code can be linked into the combined
//! `notk8s` binary (crates/notk8s) without duplicating the shared
//! dependency tree (tokio, kube, k8s-openapi, rustls) that dominates both
//! binaries' size. It changes nothing about the split: `nodeproxy` is still
//! its own crate with its own deliberately minimal dependencies, still
//! builds and ships as its own binary, and still shares no code with
//! `nodelet`.

use anyhow::{Context, Result};
use tracing::error;

pub mod config;
pub mod svc;

/// Install rustls' default CryptoProvider, unless something already did.
///
/// rustls 0.23 stopped silently picking one, and `kube::Client::try_default()`
/// panics rather than erroring without it. `install_default()` itself errors
/// on a second call, which the standalone binary can treat as impossible but
/// the combined binary cannot — hence the check rather than an `expect()`.
pub fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("installing default rustls CryptoProvider (no other provider was installed a moment ago)");
    }
}

/// Run the Service proxy until it stops.
///
/// Only returns `Err` on a condition that makes the whole process pointless
/// (no usable `nft`); otherwise it runs forever. Callers are expected to
/// exit non-zero on `Err` so a service manager's restart loop makes that
/// visible instead of leaving a live-looking process that routes nothing.
pub async fn run() -> Result<()> {
    install_crypto_provider();

    let cfg = config::Config::from_env().context("loading configuration")?;

    let client = kube::Client::try_default()
        .await
        .context("building kube client (is KUBECONFIG set and the apiserver reachable?)")?;

    if let Err(e) = svc::ServiceProxy::new(client, cfg.ip_family, cfg.lb_method).run().await {
        error!(error = ?e, "service proxy stopped");
        return Err(e);
    }
    Ok(())
}
