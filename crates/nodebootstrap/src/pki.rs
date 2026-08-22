//! Cluster PKI generation: CA, serving cert, ServiceAccount signing
//! keypair, per-component client certs. Group O's PKI half.
//!
//! Will split into `pki/{ca,serving,sa_signing,client_certs}.rs` once real
//! logic lands (single file for now -- see `docs/NODEBOOTSTRAP_PLAN.md`'s
//! scope tree for the target shape). Uses `rcgen`/`p256`/`x509-parser`/
//! `pem`, the same stack `crates/nodecontroller`'s CSR signing already
//! vetted -- see that crate's `Cargo.toml` comment for why these four.
//!
//! Deliberately does **not** borrow k3s's own generated CA the way
//! `deploy/lib/upstream-kube-apiserver.sh` does today (see that script's
//! header comment) -- minting a fresh CA here, independent of k3s, is what
//! makes `docs/NODEBOOTSTRAP_PLAN.md` point 3 (same PKI code, swap the
//! apiserver target underneath) possible at all.

use anyhow::Result;

use crate::config::Config;

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_pki {
        tracing::info!("skipping PKI generation (NODEBOOTSTRAP_SKIP_PKI)");
        return Ok(());
    }
    anyhow::bail!(
        "nodebootstrap::pki is a scaffold, not yet implemented -- see \
         docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}
