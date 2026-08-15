//! nodecontroller — kube-controller-manager's job, done event-driven where
//! upstream is, and honestly polled (not pretended away) where it isn't.
//!
//! Read `docs/CONTROLLER_MANAGER.md` first: it's the scope (every group,
//! upstream's `NewControllerDescriptors()` as source of truth, not the
//! reference docs page) and the polling architecture (`wheel.rs` +
//! `pacing.rs`) this crate is built around. Only Group A — node lifecycle —
//! is implemented so far; see that doc's "Delivery order" for what's next.
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

pub async fn run() -> Result<()> {
    let cfg = config::Config::from_env()?;
    let client = kube::Client::try_default().await.context("building apiserver client")?;

    let election_cfg = cfg.election();
    node_leaderelection::run_as_leader(client.clone(), &election_cfg, || async move {
        tracing::info!("nodecontroller is now leading — starting all controllers");
        tokio::try_join!(
            controllers::node_ipam::run(client.clone(), &cfg),
            controllers::node_lifecycle::run(client.clone(), &cfg),
        )?;
        Ok(())
    })
    .await
}
