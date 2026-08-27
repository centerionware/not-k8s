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

use anyhow::{bail, Context, Result};
use k8s_openapi::api::core::v1::{Endpoints, Service};
use kube::api::{Api, DeleteParams};

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

/// Verify the apiserver's own bootstrap-controller created the Service.
fn verify_kubernetes_service(kubeconfig: &std::path::Path) -> Result<()> {
    crate::kube_api::block_on(kubeconfig, |client| async move {
        let services: Api<Service> = Api::namespaced(client, "default");
        anyhow::ensure!(
            services.get_opt("kubernetes").await?.is_some(),
            "the 'kubernetes' Service in the default namespace is missing -- the apiserver's own \
             bootstrap-controller PostStartHook should have created it on startup; check the \
             apiserver's own logs"
        );
        tracing::info!(
            "kubernetes Service present (kube-apiserver's own bootstrap-controller, not reconciled \
             by nodebootstrap -- see this module's doc comment)"
        );
        Ok(())
    })
}

/// The first apiserver starts before CNI exists and may publish its loopback
/// fallback.  The later network-address handoff restarts the apiserver with
/// the CNI gateway, but the bootstrap controller keeps the old endpoint while
/// it reconciles multiple advertised addresses.  Remove that stale object so
/// the restarted controller can recreate one valid endpoint instead of
/// rejecting the object forever because it still contains 127.0.0.1.
pub fn reset_and_wait_for_reachable_endpoint(kubeconfig: &std::path::Path) -> Result<()> {
    crate::kube_api::block_on(kubeconfig, |client| async move {
        let endpoints: Api<Endpoints> = Api::namespaced(client, "default");
        match endpoints
            .delete("kubernetes", &DeleteParams::default())
            .await
        {
            Ok(_) => {}
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => return Err(error).context("deleting the stale kubernetes Endpoints object"),
        }

        for _ in 0..30 {
            if let Some(endpoints) = endpoints.get_opt("kubernetes").await? {
                let addresses = endpoint_addresses(&endpoints);
                if has_only_reachable_addresses(&addresses) {
                    tracing::info!(addresses = %addresses.trim(), "kubernetes Service endpoint is reachable from pods");
                    return Ok(());
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        bail!(
            "the apiserver did not recreate a reachable 'kubernetes' Endpoints object within 30s; check the kube-apiserver bootstrap-controller logs"
        )
    })
}

fn endpoint_addresses(endpoints: &Endpoints) -> String {
    endpoints
        .subsets
        .as_ref()
        .into_iter()
        .flatten()
        .flat_map(|subset| subset.addresses.as_ref().into_iter().flatten())
        .map(|address| address.ip.as_str())
        .collect::<Vec<_>>()
        .join(" ")
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
