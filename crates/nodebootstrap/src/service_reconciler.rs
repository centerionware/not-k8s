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

/// The first apiserver starts before CNI exists and may publish its loopback
/// fallback.  The later network-address handoff restarts the apiserver with
/// the CNI gateway, but the bootstrap controller keeps the old endpoint while
/// it reconciles multiple advertised addresses.  Remove that stale object so
/// the restarted controller can recreate one valid endpoint instead of
/// rejecting the object forever because it still contains 127.0.0.1.
pub fn reset_and_wait_for_reachable_endpoint(kubeconfig: &std::path::Path) -> Result<()> {
    let output = std::process::Command::new("kubectl")
        .args([
            "--kubeconfig",
            &kubeconfig.to_string_lossy(),
            "delete",
            "endpoints",
            "kubernetes",
            "-n",
            "default",
            "--ignore-not-found=true",
        ])
        .output()
        .context("deleting the stale kubernetes Endpoints object")?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to clear the stale 'kubernetes' Endpoints object: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    for _ in 0..30 {
        let output = std::process::Command::new("kubectl")
            .args([
                "--kubeconfig",
                &kubeconfig.to_string_lossy(),
                "get",
                "endpoints",
                "kubernetes",
                "-n",
                "default",
                "-o",
                "jsonpath={.subsets[*].addresses[*].ip}",
            ])
            .output()
            .context("checking the recreated kubernetes Endpoints object")?;
        let addresses = String::from_utf8_lossy(&output.stdout);
        if output.status.success() && has_only_reachable_addresses(&addresses) {
            tracing::info!(addresses = %addresses.trim(), "kubernetes Service endpoint is reachable from pods");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    anyhow::bail!(
        "the apiserver did not recreate a reachable 'kubernetes' Endpoints object within 30s; check the kube-apiserver bootstrap-controller logs"
    )
}

fn has_only_reachable_addresses(addresses: &str) -> bool {
    let mut found = false;
    for address in addresses.split_whitespace() {
        let Ok(address) = address.parse::<std::net::IpAddr>() else {
            return false;
        };
        found = true;
        if address.is_loopback() {
            return false;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::has_only_reachable_addresses;

    #[test]
    fn rejects_stale_loopback_endpoint() {
        assert!(!has_only_reachable_addresses("10.42.0.1 127.0.0.1"));
    }

    #[test]
    fn accepts_cni_gateway_endpoint() {
        assert!(has_only_reachable_addresses("10.42.0.1"));
    }
}
