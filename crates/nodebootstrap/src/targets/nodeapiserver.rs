//! Installs this repository's `nodeapiserver` in place of upstream
//! `kube-apiserver`. The target deliberately reuses the bootstrapper's
//! existing PKI and nodestore client material, so generated kubeconfigs,
//! node-side services, and the API listener all share one trust domain.

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Namespace;
use kube::api::{Api, PostParams};

use crate::config::Config;
use crate::service_mgr::{self, SupervisedService};

const SYSTEM_NAMESPACES: &[&str] = &["default", "kube-system", "kube-public", "kube-node-lease"];

pub fn run_with(cfg: &Config) -> Result<()> {
    let bin = cfg.toolchain_dir().join("bin/nodeapiserver");
    anyhow::ensure!(
        bin.exists(),
        "no nodeapiserver binary at {} -- run `nodebootstrap fetch` first",
        bin.display()
    );
    for path in [
        cfg.pki_dir().join("apiserver.crt"),
        cfg.pki_dir().join("apiserver.key"),
    ] {
        anyhow::ensure!(
            path.is_file(),
            "nodeapiserver requires PKI file {}",
            path.display()
        );
    }

    // Switching targets on an existing host must not leave two listeners
    // racing for :6443. The service manager's remove operation is safe when
    // the other target was never installed.
    service_mgr::remove(cfg, "kube-apiserver");

    let etcd_servers = super::upstream::nodestore_etcd_servers();
    super::upstream::wait_for_nodestore(&etcd_servers)?;
    let (etcd_ca, etcd_cert, etcd_key) = super::upstream::nodestore_client_pki_paths();
    install_service(
        cfg,
        &bin,
        &etcd_servers,
        &etcd_ca,
        &etcd_cert,
        &etcd_key,
        false,
    )?;
    wait_for_readyz(cfg)?;

    // Unlike kube-apiserver, the replacement has no bootstrap controller to
    // seed the standard namespaces. NamespaceLifecycle intentionally rejects
    // namespaced writes into a missing namespace, so create these before the
    // first namespaced RBAC object is applied below.
    ensure_system_namespaces(cfg)?;

    // The bootstrap objects are written through the admin kubeconfig. Start
    // without the replacement authorizer, install the RBAC bootstrap policy,
    // then restart with enforcement enabled so the steady-state service is
    // fail-closed without making the first bootstrap request impossible.
    if !cfg.skip_rbac {
        crate::rbac::run_with(cfg)?;
        install_service(
            cfg,
            &bin,
            &etcd_servers,
            &etcd_ca,
            &etcd_cert,
            &etcd_key,
            true,
        )?;
        wait_for_readyz(cfg)?;
    }
    Ok(())
}

fn ensure_system_namespaces(cfg: &Config) -> Result<()> {
    let kubeconfig = cfg.kubeconfig_dir().join("admin.kubeconfig");
    crate::kube_api::block_on(&kubeconfig, |client| async move {
        let namespaces: Api<Namespace> = Api::all(client);
        for name in SYSTEM_NAMESPACES {
            if namespaces.get_opt(name).await?.is_some() {
                continue;
            }
            let namespace = Namespace {
                metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                    name: Some((*name).to_string()),
                    ..Default::default()
                },
                ..Default::default()
            };
            match namespaces.create(&PostParams::default(), &namespace).await {
                Ok(_) => tracing::info!(
                    namespace = *name,
                    "created replacement-apiserver system namespace"
                ),
                Err(kube::Error::Api(error)) if error.code == 409 => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("creating system namespace {name}"));
                }
            }
        }
        Ok(())
    })
}

fn install_service(
    cfg: &Config,
    bin: &std::path::Path,
    etcd_servers: &str,
    etcd_ca: &std::path::Path,
    etcd_cert: &std::path::Path,
    etcd_key: &std::path::Path,
    enforce_rbac: bool,
) -> Result<()> {
    let pki_dir = cfg.pki_dir();
    let mut values = vec![
        ("NODEAPISERVER_BIND_ADDR", "0.0.0.0:6443".to_string()),
        ("NODEAPISERVER_NODESTORE_ENDPOINT", etcd_servers.to_string()),
        (
            "NODEAPISERVER_NODESTORE_CA_FILE",
            etcd_ca.to_string_lossy().into_owned(),
        ),
        (
            "NODEAPISERVER_NODESTORE_CERT_FILE",
            etcd_cert.to_string_lossy().into_owned(),
        ),
        (
            "NODEAPISERVER_NODESTORE_KEY_FILE",
            etcd_key.to_string_lossy().into_owned(),
        ),
        (
            "NODEAPISERVER_TLS_CERT_FILE",
            pki_dir.join("apiserver.crt").to_string_lossy().into_owned(),
        ),
        (
            "NODEAPISERVER_TLS_KEY_FILE",
            pki_dir.join("apiserver.key").to_string_lossy().into_owned(),
        ),
        (
            "NODEAPISERVER_CLIENT_CA_FILE",
            pki_dir.join("ca.crt").to_string_lossy().into_owned(),
        ),
        (
            "NODEAPISERVER_SERVICE_ACCOUNT_SIGNING_KEY_FILE",
            pki_dir.join("sa.key").to_string_lossy().into_owned(),
        ),
        (
            "NODEAPISERVER_SERVICE_ACCOUNT_ISSUER",
            format!("https://kubernetes.default.svc.{}", cfg.cluster_domain()),
        ),
        (
            "NODEAPISERVER_KUBELET_CLIENT_CERT_FILE",
            pki_dir
                .join("kube-apiserver.crt")
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "NODEAPISERVER_KUBELET_CLIENT_KEY_FILE",
            pki_dir
                .join("kube-apiserver.key")
                .to_string_lossy()
                .into_owned(),
        ),
        ("NOTK8S_COMPONENT", "nodeapiserver".to_string()),
        (
            "NOTK8S_COMPONENT_BINARY",
            bin.to_string_lossy().into_owned(),
        ),
    ];
    if enforce_rbac {
        values.push(("NODEAPISERVER_ENFORCE_RBAC", "1".to_string()));
    }
    let env: Vec<(&str, &str)> = values
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let exec_cmd = bin.to_string_lossy().into_owned();
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodeapiserver",
            description: "nodeapiserver -- not-k8s Kubernetes API server",
            exec_cmd: &exec_cmd,
            after: Some("nodestore.service"),
            env: &env,
        },
    )
    .context("installing nodeapiserver as a supervised service")?;
    Ok(())
}

/// Nodeapiserver is configured with the kubelet client identity from its
/// first start, so it has no upstream two-phase certificate handoff.
pub fn enable_nodelet_proxy(_cfg: &Config) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SYSTEM_NAMESPACES;

    #[test]
    fn replacement_apiserver_seeds_the_standard_namespaces() {
        assert_eq!(
            SYSTEM_NAMESPACES,
            &["default", "kube-system", "kube-public", "kube-node-lease"]
        );
    }
}

/// The replacement apiserver does not run upstream's bootstrap-controller.
/// Refresh the endpoint object after CNI has assigned a reachable bridge
/// address; `service_reconciler` owns the object contents for this target.
pub fn refresh_network_advertise_address(cfg: &Config) -> Result<()> {
    let needs_cni_seed = cfg.advertise_address.is_none();
    if needs_cni_seed {
        // A replacement apiserver does not have upstream's bootstrap Pod to
        // cause the first CNI network namespace to be created. Reuse the
        // shared seed path so the bridge exists before publishing the
        // in-cluster endpoint.
        super::upstream::ensure_cni_seed_pod(cfg)?;
    }
    let address = cfg
        .advertise_address
        .clone()
        .map(Ok)
        .unwrap_or_else(super::upstream::wait_for_cni_address);
    let result = address.and_then(|address| {
        crate::service_reconciler::reconcile_nodeapiserver_endpoint(cfg, &address)
    });
    if needs_cni_seed {
        super::upstream::remove_cni_seed_pod(cfg);
    }
    result
}

fn wait_for_readyz(cfg: &Config) -> Result<()> {
    let agent = match super::upstream::trusting_agent(&cfg.pki_dir().join("ca.crt")) {
        Ok(agent) => agent,
        Err(error) => {
            tracing::warn!(?error, "could not build a nodeapiserver readiness client");
            return Err(error).context("building the nodeapiserver readiness client");
        }
    };
    for _ in 0..30 {
        match agent.get("https://127.0.0.1:6443/readyz").call() {
            Ok(_) | Err(ureq::Error::Status(_, _)) => {
                tracing::info!("nodeapiserver is answering requests");
                return Ok(());
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_secs(1)),
        }
    }
    anyhow::bail!(
        "nodeapiserver did not answer /readyz within 30s; check the nodeapiserver service logs"
    )
}
