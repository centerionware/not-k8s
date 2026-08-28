//! Installs this repository's `nodeapiserver` in place of upstream
//! `kube-apiserver`. The target deliberately reuses the bootstrapper's
//! existing PKI and nodestore client material, so generated kubeconfigs,
//! node-side services, and the API listener all share one trust domain.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::service_mgr::{self, SupervisedService};

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

/// The replacement apiserver does not run upstream's bootstrap-controller.
/// Refresh the endpoint object after CNI has assigned a reachable bridge
/// address; `service_reconciler` owns the object contents for this target.
pub fn refresh_network_advertise_address(cfg: &Config) -> Result<()> {
    let address = cfg
        .advertise_address
        .clone()
        .or_else(detect_cni_address)
        .context("nodeapiserver needs --advertise-address or a CNI bridge address to publish default/kubernetes")?;
    crate::service_reconciler::reconcile_nodeapiserver_endpoint(cfg, &address)
}

fn detect_cni_address() -> Option<String> {
    std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "cni0"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .nth(3)
                .and_then(|cidr| cidr.split('/').next())
                .map(str::to_owned)
        })
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
