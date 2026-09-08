//! The `kubernetes` default Service + endpoint reconciler -- what makes
//! `kubernetes.default.svc` resolve to the apiserver's own reachable
//! address/port from inside the cluster.
//!
//! **Finding (2026-08-22), same shape as `rbac.rs`'s:** for the upstream
//! target this is not a separate component to build at all. Real upstream `kube-apiserver`
//! reconciles the `kubernetes` Service/Endpoints itself, unconditionally,
//! via a `PostStartHook` (`bootstrap-controller`, wired in
//! `pkg/controlplane/instance.go`, running `Controller.RunKubernetesService`
//! from `pkg/controlplane/controller.go`) -- driven by `--advertise-address`
//! and `--secure-port`, the same two flags `targets/upstream.rs` already
//! sets. No RBAC gate, no opt-in flag: any real `kube-apiserver`, k3s's
//! embedded one included, has always done this on its own. This module is
//! therefore the same shape as `rbac.rs` for that target: a thin
//! verify-it-happened check. The nodeapiserver target has no upstream
//! bootstrap-controller, so it uses the small explicit reconciler below.

use anyhow::{bail, Context, Result};
use k8s_openapi::api::core::v1::{Endpoints, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use serde_json::json;

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
    if matches!(cfg.target, crate::config::Target::NodeApiserver) {
        let address = cfg.advertise_address.as_deref().unwrap_or("127.0.0.1");
        return reconcile_nodeapiserver_endpoint(cfg, address);
    }
    verify_kubernetes_service(&kubeconfig)
}

/// Nodeapiserver intentionally does not embed upstream's bootstrap-controller.
/// Create the one control-plane-owned Service and endpoint that every in-cluster
/// client expects, then let the later CNI handoff replace the temporary
/// loopback endpoint with a reachable node address.
pub fn reconcile_nodeapiserver_endpoint(cfg: &Config, address: &str) -> Result<()> {
    let kubeconfig = cfg.kubeconfig_dir().join("admin.kubeconfig");
    let service_ip = cfg.service_ip()?.to_string();
    let service_ips = cfg
        .service_ips()?
        .into_iter()
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>();
    let endpoint_address = address.to_string();
    crate::kube_api::block_on(&kubeconfig, move |client| async move {
        let services: Api<Service> = Api::namespaced(client.clone(), "default");
        let endpoints: Api<Endpoints> = Api::namespaced(client.clone(), "default");
        let endpoint_slices: Api<EndpointSlice> = Api::namespaced(client, "default");
        if services.get_opt("kubernetes").await?.is_none() {
            let service: Service = serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {"name": "kubernetes", "namespace": "default"},
                "spec": {
                    "type": "ClusterIP",
                    "clusterIP": service_ip,
                    "clusterIPs": service_ips,
                    "ports": [{"name": "https", "port": 443, "protocol": "TCP", "targetPort": 6443}]
                }
            }))?;
            services
                .create(&PostParams::default(), &service)
                .await
                .context("creating default/kubernetes Service for nodeapiserver")?;
        } else {
            // Match upstream's in-cluster Service contract even when an older
            // nodeapiserver install left the service port at 6443. The
            // ClusterIP is immutable and is deliberately not included in
            // this merge patch.
            services
                .patch(
                    "kubernetes",
                    &PatchParams::default(),
                    &Patch::Merge(&json!({
                        "spec": {
                            "ports": [{"name": "https", "port": 443, "protocol": "TCP", "targetPort": 6443}]
                        }
                    })),
                )
                .await
                .context("aligning default/kubernetes Service ports for nodeapiserver")?;
        }

        let endpoint: Endpoints = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Endpoints",
            "metadata": {"name": "kubernetes", "namespace": "default"},
            "subsets": [{
                "addresses": [{"ip": endpoint_address.clone()}],
                "ports": [{"name": "https", "port": 6443, "protocol": "TCP"}]
            }]
        }))?;
        endpoints
            .patch(
                "kubernetes",
                &PatchParams::apply("nodebootstrap"),
                &Patch::Apply(&endpoint),
            )
            .await
            .context("publishing the nodeapiserver default/kubernetes endpoint")?;

        // Upstream's endpoint reconciler also exposes the control-plane
        // backend as an EndpointSlice. nodeproxy watches EndpointSlices, so
        // publishing only the legacy Endpoints object leaves the Service
        // unroutable and keeps CoreDNS readiness false on a fresh install.
        let endpoint_slice: EndpointSlice = serde_json::from_value(json!({
            "apiVersion": "discovery.k8s.io/v1",
            "kind": "EndpointSlice",
            "metadata": {
                "name": "kubernetes",
                "namespace": "default",
                "labels": {
                    "kubernetes.io/service-name": "kubernetes",
                    "endpointslice.kubernetes.io/managed-by": "nodebootstrap"
                }
            },
            "addressType": if endpoint_address.contains(':') { "IPv6" } else { "IPv4" },
            "ports": [{"name": "https", "port": 6443, "protocol": "TCP"}],
            "endpoints": [{
                "addresses": [endpoint_address],
                "conditions": {"ready": true, "serving": true, "terminating": false}
            }]
        }))?;
        endpoint_slices
            .patch(
                "kubernetes",
                &PatchParams::apply("nodebootstrap").force(),
                &Patch::Apply(&endpoint_slice),
            )
            .await
            .context("publishing the nodeapiserver default/kubernetes EndpointSlice")?;
        tracing::info!(address = %endpoint.subsets.as_ref().and_then(|subsets| subsets.first()).and_then(|subset| subset.addresses.as_ref()).and_then(|addresses| addresses.first()).map(|address| address.ip.as_str()).unwrap_or("unknown"), "reconciled nodeapiserver kubernetes Service endpoint");
        Ok(())
    })
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
