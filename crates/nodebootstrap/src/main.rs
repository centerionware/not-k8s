//! nodebootstrap — cluster bootstrap: toolchain/containerd/CNI setup,
//! build-or-fetch, and PKI/RBAC/kubeconfig generation. Replaces
//! `deploy/bootstrap-source.sh` / `deploy/bootstrap-release.sh` and most of
//! `deploy/lib/*.sh`. See `docs/NODEBOOTSTRAP_PLAN.md` for the scope and
//! phasing decision this crate follows.
//!
//! A one-shot CLI, not a long-running service like the rest of not-k8s's
//! components: it runs to completion and exits, same shape as the shell
//! entry points it replaces. No tokio — everything here is either a
//! filesystem write or a subprocess call, both synchronous.
//!
//! Everything below the process boundary lives in `lib.rs`, so the combined
//! `notk8s` binary (once this crate is wired into it) can run exactly this
//! code path without a second copy of the dependency tree.

use anyhow::{bail, Result};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    // rustls 0.23 needs a process-wide CryptoProvider installed once
    // before any TLS use (`targets/upstream.rs`'s readyz check builds a
    // rustls::ClientConfig directly); without this it panics on first use
    // rather than picking one automatically, even with only the "ring"
    // feature enabled -- confirmed for real (round 1 of this crate's own
    // e2e workflow, panicked mid-run with exactly this message).
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("a rustls CryptoProvider was already installed (unexpected -- this is the only place nodebootstrap installs one)"))?;

    let cmd = std::env::args().nth(1);
    match cmd.as_deref() {
        Some("toolchain") => nodebootstrap::toolchain::run(),
        Some("containerd") => nodebootstrap::containerd::run(),
        Some("cni") => nodebootstrap::cni::run(),
        Some("fetch") => nodebootstrap::fetch::run(),
        Some("pki") => nodebootstrap::pki::run(),
        Some("kubeconfig") => nodebootstrap::kubeconfig::run(),
        Some("targets") => {
            let cfg = nodebootstrap::config::Config::from_env()?;
            nodebootstrap::targets::run_with(&cfg)
        }
        Some("rbac") => nodebootstrap::rbac::run(),
        Some("service-reconciler") => nodebootstrap::service_reconciler::run(),
        Some("manifests") => nodebootstrap::manifests::run(),
        Some("all") | None => nodebootstrap::run_all(),
        Some(other) => bail!(
            "unknown subcommand '{other}' — one of: toolchain, containerd, cni, fetch, pki, \
             kubeconfig, targets, rbac, service-reconciler, manifests, all"
        ),
    }
}
