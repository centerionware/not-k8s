//! The ~90 `system:` ClusterRoles/ClusterRoleBindings from upstream's
//! `bootstrappolicy` (`plugin/pkg/auth/authorizer/rbac/bootstrappolicy` in
//! `kubernetes/kubernetes`). Group O's RBAC half.
//!
//! **Finding (2026-08-22, verified against this project's own existing
//! deploy):** this module does not need to vendor or hand-build that list
//! at all. `deploy/setup-control-plane.sh` -- which has run a real
//! kube-apiserver (k3s's own embedded one) with `--authorization-mode`
//! including RBAC for as long as this project has existed -- contains zero
//! `kubectl create clusterrole`/`clusterrolebinding` calls, because
//! upstream `kube-apiserver` creates and reconciles the entire bootstrap
//! policy itself: a `PostStartHook` (`rbac/bootstrap-roles`, wired in
//! `pkg/controlplane` via `storage_rbac.go`'s `PostStartHook`) runs on
//! every apiserver start whenever `--authorization-mode` includes `RBAC`,
//! and it is a *reconciler* -- it re-applies the ~90 objects on every
//! restart, not a one-time install. k3s "bootstrapping RBAC for us" was
//! never k3s-specific behavior; it was this same PostStartHook running
//! inside k3s's embedded real apiserver process the whole time. Starting
//! real upstream `kube-apiserver` (`targets/upstream.rs`) with
//! `--authorization-mode=Node,RBAC` gets this for free, identically.
//!
//! What's left for this module to actually do: confirm the apiserver was
//! started with RBAC enabled and that the reconciler actually ran (a smoke
//! check, not a re-implementation), and apply anything genuinely specific
//! to *this* project that upstream's own bootstrap policy has no opinion
//! on -- e.g. any not-k8s-component-specific bindings that aren't already
//! covered by the built-in `system:kube-controller-manager`/
//! `system:kube-scheduler`/`system:node` identities `pki.rs` already issues
//! certs for. None identified yet; revisit once `nodescheduler`/
//! `nodecontroller` are actually run against a `nodebootstrap`-bootstrapped
//! cluster and something is denied that shouldn't be.

use anyhow::{Context, Result};

use crate::config::Config;

/// A handful of the ~90 bootstrap `system:` ClusterRoles that must exist if
/// the PostStartHook ran at all -- not the full list (that would just be
/// re-deriving upstream's own table, the exact duplication this module's
/// doc comment explains is unnecessary), just enough to catch "RBAC wasn't
/// actually enabled" or "the apiserver never became ready" with a clear
/// error instead of a mysterious later 403.
const SENTINEL_CLUSTER_ROLES: &[&str] =
    &["cluster-admin", "system:node", "system:discovery", "system:kube-scheduler"];

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_rbac {
        tracing::info!("skipping RBAC bootstrap verification (NODEBOOTSTRAP_SKIP_RBAC)");
        return Ok(());
    }
    let kubeconfig = cfg.kubeconfig_dir().join("admin.kubeconfig");
    verify_bootstrap_rbac(&kubeconfig)
}

/// Shells out to `kubectl get clusterrole <name>` for each sentinel role
/// using the admin kubeconfig `kubeconfig.rs` wrote. A `kube` crate client
/// would be more idiomatic than shelling out, but every other install-time
/// check in this crate (`toolchain.rs`, `containerd.rs`) is already a
/// subprocess call, and pulling in the `kube`/`k8s-openapi`/tokio stack
/// into a one-shot CLI whose other checks are all synchronous subprocess
/// calls is not worth the async runtime it would drag in for one caller.
fn verify_bootstrap_rbac(kubeconfig: &std::path::Path) -> Result<()> {
    for role in SENTINEL_CLUSTER_ROLES {
        let status = std::process::Command::new("kubectl")
            .args(["--kubeconfig", &kubeconfig.to_string_lossy(), "get", "clusterrole", role])
            .output()
            .with_context(|| format!("running kubectl to check for ClusterRole {role}"))?;
        if !status.status.success() {
            anyhow::bail!(
                "bootstrap ClusterRole '{role}' is missing after the apiserver started -- either \
                 --authorization-mode didn't include RBAC, or the apiserver never reached ready. \
                 kubectl stderr: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        }
    }
    tracing::info!(
        checked = SENTINEL_CLUSTER_ROLES.len(),
        "bootstrap RBAC policy present (kube-apiserver's own PostStartHook, not vendored here -- \
         see this module's doc comment)"
    );
    Ok(())
}
