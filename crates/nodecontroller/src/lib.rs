//! nodecontroller — kube-controller-manager's job, done event-driven where
//! upstream is, and honestly polled (not pretended away) where it isn't.
//!
//! Read `docs/CONTROLLER_MANAGER.md` first: it's the scope (every group,
//! upstream's `NewControllerDescriptors()` as source of truth, not the
//! reference docs page) and the polling architecture (`wheel.rs` +
//! `pacing.rs`) this crate is built around. Group A (node lifecycle),
//! Group B (service routing, `endpointslice-controller`), and the
//! object-count slice of Group D (`resourcequota-controller`) are
//! implemented, plus the minimum slice of Group C
//! (`serviceaccount-controller` only) needed to unblock testing Group A at
//! all — see `controllers/service_account.rs`'s and
//! `controllers/resource_quota.rs`'s own module docs for why each is
//! scoped the way it is. See `docs/CONTROLLER_MANAGER.md`'s "Delivery
//! order" for what's next (garbage-collector-controller is deliberately
//! deferred until Group E exists — see Group D's own doc for why).
//!
//! Single leader-election lease (`kube-system/kube-controller-manager`,
//! matching upstream's own name — see `config.rs`) covers the whole
//! process, the same as upstream elects once rather than per-controller:
//! every controller in this crate is a single writer of its own objects,
//! and standing that up twice is a race, not redundancy — the same
//! reasoning `nodescheduler`'s leader election documents for `Binding`.

pub mod config;
pub mod controllers;
pub mod jitter;
pub mod pacing;
pub mod watch;
pub mod wheel;

use anyhow::{Context, Result};

/// Install rustls' default `CryptoProvider`, unless something already did.
///
/// rustls 0.23 stopped silently picking one, and `kube::Client::try_default()`
/// panics rather than erroring without it — confirmed live in CI (e2e.yml
/// run 31875853444): `nodecontroller.service` crash-looped on exactly this
/// panic, since this crate was the one place that copy-pasting
/// `nodescheduler`'s/`nodeproxy`'s/`nodelet`'s own `install_crypto_provider()`
/// got missed. `install_default()` itself errors on a second call, which a
/// standalone binary can treat as impossible but the combined `notk8s`
/// binary cannot (every component's `run()` could be reached in-process) —
/// hence the check rather than an `expect()` alone.
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("installing default rustls CryptoProvider (no other provider was installed a moment ago)");
    }
}

pub async fn run() -> Result<()> {
    install_crypto_provider();

    let cfg = config::Config::from_env()?;
    let client = kube::Client::try_default().await.context("building apiserver client")?;

    let election_cfg = cfg.election();
    node_leaderelection::run_as_leader(client.clone(), &election_cfg, || async move {
        tracing::info!("nodecontroller is now leading — starting all controllers");
        tokio::try_join!(
            controllers::node_ipam::run(client.clone(), &cfg),
            controllers::node_lifecycle::run(client.clone(), &cfg),
            controllers::service_account::run(client.clone(), &cfg),
            controllers::endpoint_slice::run(client.clone(), &cfg),
            controllers::resource_quota::run(client.clone(), &cfg),
        )?;
        Ok(())
    })
    .await
}
