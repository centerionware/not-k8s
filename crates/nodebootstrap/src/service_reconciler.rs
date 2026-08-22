//! The `kubernetes` default Service + endpoint reconciler -- what makes
//! `kubernetes.default.svc` resolve to the apiserver's own reachable
//! address/port from inside the cluster.
//!
//! **Finding (2026-08-22), same shape as `rbac.rs`'s:** this is not a
//! separate component to build at all. Real upstream `kube-apiserver`
//! reconciles the `kubernetes` Service/Endpoints itself, unconditionally,
//! via a `PostStartHook` (`bootstrap-controller`, wired in
//! `pkg/controlplane/instance.go`, running `Controller.RunKubernetesService`
//! from `pkg/controlplane/controller.go`) -- driven by `--advertise-address`
//! and `--secure-port`, the same two flags `targets/upstream.rs` already
//! sets. No RBAC gate, no opt-in flag: any real `kube-apiserver`, k3s's
//! embedded one included, has always done this on its own. This module is
//! therefore the same shape as `rbac.rs`: a thin verify-it-happened check,
//! not a reconciler this crate runs itself.

use anyhow::{Context, Result};

use crate::config::Config;

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_service_reconciler {
        tracing::info!(
            "skipping kubernetes Service verification (NODEBOOTSTRAP_SKIP_SERVICE_RECONCILER)"
        );
        return Ok(());
    }
    let kubeconfig = cfg.kubeconfig_dir().join("admin.kubeconfig");
    verify_kubernetes_service(&kubeconfig)
}

/// `kubectl get service kubernetes -n default` -- same subprocess-call
/// posture `rbac.rs`/`manifests.rs` already use and explain.
fn verify_kubernetes_service(kubeconfig: &std::path::Path) -> Result<()> {
    let output = std::process::Command::new("kubectl")
        .args(["--kubeconfig", &kubeconfig.to_string_lossy(), "get", "service", "kubernetes", "-n", "default"])
        .output()
        .context("running kubectl to check for the kubernetes Service")?;
    if !output.status.success() {
        anyhow::bail!(
            "the 'kubernetes' Service in the default namespace is missing -- the apiserver's own \
             bootstrap-controller PostStartHook should have created it on startup; check the \
             apiserver's own logs. kubectl stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    tracing::info!(
        "kubernetes Service present (kube-apiserver's own bootstrap-controller, not reconciled \
         by nodebootstrap -- see this module's doc comment)"
    );
    Ok(())
}
