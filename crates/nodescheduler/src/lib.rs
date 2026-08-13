//! nodescheduler — pod placement for not-k8s.
//!
//! kube-scheduler's job. See `main.rs` for the standalone binary,
//! `docs/SCHEDULER.md` for the design and the parity scope, and `cycle.rs`
//! for the invariants the scheduling cycle is built on.
//!
//! This is a library so the same code links into the combined `notk8s` binary
//! (crates/notk8s) without a second copy of the shared dependency tree. It
//! changes nothing about the split: `nodescheduler` is still its own crate
//! with its own deliberately minimal dependencies, still ships as its own
//! binary, and still shares no code with `nodelet` or `nodeproxy`.
//!
//! # Build order
//!
//! Phase 1 (see docs/SCHEDULER.md) is landing in pieces. What is here now:
//! the event vocabulary, the plugin framework and the default plugin set, the
//! projection cache and incremental snapshot, and the event-driven queue. The
//! scheduling cycle, binding cycle, leader election and watch layer are the
//! remaining pieces, and until they exist [`run`] refuses to start rather than
//! pretending to schedule.

use anyhow::{Context, Result};

pub mod cache;
pub mod config;
pub mod events;
pub mod framework;
pub mod queue;

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

/// Run the scheduler until it stops.
///
/// Only returns `Err` on a condition that makes the whole process pointless
/// (an unreachable apiserver at startup, an unparseable configuration);
/// otherwise it runs forever. Every caller returns that error straight out of
/// `main`, which both prints it and exits non-zero, so a service manager's
/// restart loop makes the failure visible instead of leaving a live-looking
/// process that schedules nothing.
pub async fn run() -> Result<()> {
    install_crypto_provider();

    let cfg = config::Config::from_env().context("loading configuration")?;

    let client = kube::Client::try_default()
        .await
        .context("building kube client (is KUBECONFIG set and the apiserver reachable?)")?;

    // Deliberately loud and deliberately fatal. A scheduler that starts,
    // registers, holds the leader lease and then schedules nothing is far
    // worse than one that refuses to start: the cluster looks healthy while
    // every new pod sits Pending, and `--disable-scheduler` means k3s's own
    // scheduler is not there to cover for it.
    let _ = client;
    anyhow::bail!(
        "nodescheduler is not finished: the scheduling cycle, binding cycle, leader \
         election and watch layer are still being written (see docs/SCHEDULER.md, \
         \"Phasing\"). Refusing to start rather than holding the leader lease and \
         placing nothing. Run with --scheduler=none, which leaves pod placement to \
         the kube-scheduler k3s runs itself."
    );
}
