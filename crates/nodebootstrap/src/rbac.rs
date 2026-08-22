//! The ~90 `system:` ClusterRoles/ClusterRoleBindings from upstream's
//! `bootstrappolicy` (`plugin/pkg/auth/authorizer/rbac/bootstrappolicy` in
//! `kubernetes/kubernetes`). Group O's RBAC half.
//!
//! Real logic to land here: vendor (or transcribe, with the exact upstream
//! ref recorded, same discipline `APISERVER_PLAN.md` finding 5's vendored
//! OpenAPI specs use) the policy list, apply it via the generated
//! kubeconfig once the target apiserver (`targets.rs`) is reachable. Will
//! split into `rbac/{roles,bindings}.rs` once the real list lands.

use anyhow::Result;

use crate::config::Config;

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_rbac {
        tracing::info!("skipping RBAC bootstrap policy (NODEBOOTSTRAP_SKIP_RBAC)");
        return Ok(());
    }
    anyhow::bail!(
        "nodebootstrap::rbac is a scaffold, not yet implemented -- see \
         docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}
