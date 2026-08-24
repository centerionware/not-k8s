//! Installs this project's own components -- `nodestore`, `nodescheduler`,
//! `nodecontroller`, `nodelet`, `nodeproxy` -- as real, persistent,
//! auto-restarting services via `service_mgr.rs`. Replaces
//! `deploy/lib/{nodestore,nodelet,nodeproxy,nodescheduler,
//! nodecontroller}-service.sh`.
//!
//! **Binary location**: every one of `fetch.rs`'s two paths
//! (`Source::Compile`/`Source::Release`) now stages its output at
//! `Config::toolchain_dir()/bin/<component-name>` -- one canonical
//! location this module looks in regardless of how the binary got there.
//!
//! **`nodescheduler`/`nodecontroller` are the default, not upstream's
//! binaries** (decided 2026-08-22, user direction): `targets/upstream.rs`
//! installs only `kube-apiserver` now, specifically because these two
//! already exist and are already built on `main` -- there is no reason to
//! run the upstream `kube-scheduler`/`kube-controller-manager` binaries
//! they exist to replace. They use the `kube-scheduler.kubeconfig`/
//! `kube-controller-manager.kubeconfig` `pki.rs`/`kubeconfig.rs` already
//! mint for exactly those two identities (`system:kube-scheduler`/
//! `system:kube-controller-manager`) -- not `admin.kubeconfig`, unlike
//! `nodelet`/`nodeproxy` below.
//!
//! **`nodelet`/`nodeproxy` use `admin.kubeconfig`** -- matching current
//! production behavior exactly (`bootstrap-source.sh` points them at the
//! same admin/cluster-admin kubeconfig today; there is no existing
//! per-component RBAC restriction to preserve or regress here).
//! Tightening this to a real `system:node:<name>` cert for `nodelet` via
//! `nodecontroller`'s own CSR-signing flow would be a new improvement over
//! current behavior, not something this port owes -- tracked as
//! follow-up, not a gap introduced here.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::service_mgr::{self, SupervisedService};

fn binary_path(cfg: &Config, name: &str) -> std::path::PathBuf {
    cfg.toolchain_dir().join("bin").join(name)
}

/// Forwards every `<prefix>_*` env var already set in nodebootstrap's own
/// process environment into the installed unit, plus `RUST_LOG` if set --
/// same `<PREFIX>_*` convention `nodescheduler-service.sh`'s/
/// `nodecontroller-service.sh`'s/`nodestore-service.sh`'s own
/// `*_env_lines()` functions use (`compgen -v | grep '^<PREFIX>_'`).
/// `RUST_LOG` forwarding itself only existed in the scheduler/controller
/// shell versions (`nodestore-service.sh` never had it); added here for
/// all three callers of this helper for consistency -- a superset of the
/// old behavior, not a regression, since a caller not setting `RUST_LOG`
/// is unaffected. Without this helper, a caller (an operator's shell, or a
/// CI workflow) exporting e.g. `NODECONTROLLER_DISABLED_CONTROLLERS` or
/// `RUST_LOG=nodescheduler=debug` before invoking `nodebootstrap` would see
/// it silently dropped -- `SupervisedService.env` is otherwise a fixed list
/// each `ensure_*` builds itself, with no passthrough of its own.
///
/// `nodelet`/`nodeproxy` deliberately don't call this: their shell
/// equivalents (`nodelet-service.sh`/`nodeproxy-service.sh`) never forwarded
/// a `NODELET_*`/`NODEPROXY_*` prefix or `RUST_LOG` either, so not adding it
/// here isn't a regression -- only `nodestore`/`nodescheduler`/
/// `nodecontroller` had this in the shell version.
fn forwarded_env(prefix: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> =
        std::env::vars().filter(|(k, v)| k.starts_with(prefix) && !v.is_empty()).collect();
    out.sort();
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        if !rust_log.is_empty() {
            out.push(("RUST_LOG".to_string(), rust_log));
        }
    }
    out
}

/// Every component this module owns, in the same order `run_all()` calls
/// them in directly (not through this function -- `run_all()` interleaves
/// `nodescheduler`/`nodecontroller` with `targets`/`cni`/`rbac`/etc., so it
/// calls each `ensure_*` itself rather than going through this bundle).
/// This is the convenience entry point for the `nodebootstrap services`
/// subcommand, run standalone.
pub fn run_with(cfg: &Config) -> Result<()> {
    ensure_nodestore(cfg)?;
    ensure_nodescheduler(cfg)?;
    ensure_nodecontroller(cfg)?;
    ensure_nodelet(cfg)?;
    ensure_nodeproxy(cfg)?;
    Ok(())
}

/// Installed and started **before** `targets::run_with` in `run_all()` --
/// `targets/upstream.rs` already orders `kube-apiserver.service` `After=
/// nodestore.service`, so nodestore has to actually exist and be enabled
/// by the time that runs, or the apiserver comes up with nothing to talk
/// to.
pub fn ensure_nodestore(cfg: &Config) -> Result<()> {
    let bin = binary_path(cfg, "nodestore");
    anyhow::ensure!(bin.exists(), "no nodestore binary at {} -- run `nodebootstrap fetch` first", bin.display());

    let listen = std::env::var("NODESTORE_LISTEN").unwrap_or_else(|_| "127.0.0.1:2379".to_string());
    let data_dir = std::env::var("NODESTORE_DATA_DIR").unwrap_or_else(|_| "/var/lib/nodestore".to_string());
    let forwarded = forwarded_env("NODESTORE_");
    let binary = bin.to_string_lossy().to_string();
    let mut env: Vec<(&str, &str)> = vec![
        ("NODESTORE_LISTEN", &listen),
        ("NODESTORE_DATA_DIR", &data_dir),
        ("NOTK8S_COMPONENT", "nodestore"),
        ("NOTK8S_COMPONENT_BINARY", &binary),
    ];
    for (k, v) in &forwarded {
        // NODESTORE_LISTEN/NODESTORE_DATA_DIR are already set above with
        // their own defaults applied -- skip re-adding them verbatim so
        // the unit doesn't carry two Environment= lines for the same name.
        if k == "NODESTORE_LISTEN" || k == "NODESTORE_DATA_DIR" {
            continue;
        }
        env.push((k.as_str(), v.as_str()));
    }
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodestore",
            description: "nodestore -- not-k8s datastore (etcd v3 API over sqlite)",
            exec_cmd: &bin.to_string_lossy(),
            after: None, // nodestore is what other units order *after*, not the reverse
            env: &env,
        },
    )
    .context("installing nodestore as a supervised service")
}

/// Called last in `run_all()`, after containerd/CNI (`containerd.rs`/
/// `cni.rs`) and a reachable apiserver (`targets::run_with`) -- `nodelet`
/// needs both to be meaningfully useful, same ordering `bootstrap-
/// source.sh` uses today.
pub fn ensure_nodelet(cfg: &Config) -> Result<()> {
    if cfg.skip_nodelet {
        tracing::info!("NODEBOOTSTRAP_SKIP_NODELET=1 -- skipping nodelet");
        return Ok(());
    }
    let bin = binary_path(cfg, "nodelet");
    anyhow::ensure!(bin.exists(), "no nodelet binary at {} -- run `nodebootstrap fetch` first", bin.display());
    let kubeconfig = if cfg.worker {
        cfg.worker_bootstrap_kubeconfig
            .clone()
            .or_else(|| cfg.worker_kubeconfig.clone())
            .context("worker mode requires a kubeconfig for the existing control plane")?
    } else {
        cfg.cluster_kubeconfig()?
    };
    let kubeconfig = kubeconfig.to_string_lossy().to_string();
    let runtime = cfg.nodelet_runtime();
    let server_cert_dir = cfg.nodelet_server_cert_dir().to_string_lossy().to_string();
    let client_ca_file = std::env::var("NODELET_CLIENT_CA_FILE").ok().or_else(|| {
        (!cfg.worker).then(|| cfg.pki_dir().join("ca.crt").to_string_lossy().to_string())
    });
    let nodelet_kubeconfig = cfg.worker_nodelet_kubeconfig().to_string_lossy().to_string();
    let bootstrap_kubeconfig = cfg
        .worker_bootstrap_kubeconfig
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let node_name = cfg.node_name();
    let binary = bin.to_string_lossy().to_string();
    let mut env = vec![
        ("KUBECONFIG", kubeconfig.as_str()),
        ("NODELET_RUNTIME", runtime.as_str()),
        ("NODELET_SERVER_CERT_DIR", server_cert_dir.as_str()),
        ("NODELET_NODE_NAME", node_name.as_str()),
        ("NOTK8S_COMPONENT", "nodelet"),
        ("NOTK8S_COMPONENT_BINARY", binary.as_str()),
    ];
    if let Some(client_ca_file) = client_ca_file.as_deref() {
        env.push(("NODELET_CLIENT_CA_FILE", client_ca_file));
    }
    if let Some(bootstrap) = bootstrap_kubeconfig.as_deref() {
        env.push(("NODELET_BOOTSTRAP_KUBECONFIG", bootstrap));
        env.push(("NODELET_KUBECONFIG", nodelet_kubeconfig.as_str()));
    }
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodelet",
            description: "nodelet -- not-k8s node agent (kubelet replacement)",
            exec_cmd: &bin.to_string_lossy(),
            after: (!cfg.worker).then_some("kube-apiserver.service"),
            env: &env,
        },
    )
    .context("installing nodelet as a supervised service")
}

/// A control-plane-only join must not leave a worker service from an earlier
/// single-node install active on a reused host.
pub fn remove_nodelet(cfg: &Config) {
    service_mgr::remove(cfg, "nodelet");
}

/// Called last in `run_all()`, alongside `ensure_nodelet` (same ordering
/// reasoning). Skipped entirely when `NODEBOOTSTRAP_PROXY=none` (this
/// project's Service routing may be something else -- Cilium, a real
/// kube-proxy -- or nothing), same as every other skip flag being checked
/// inside its own module's entry point rather than at `run_all()`'s call
/// site.
pub fn ensure_nodeproxy(cfg: &Config) -> Result<()> {
    if cfg.skip_nodeproxy {
        service_mgr::remove(cfg, "nodeproxy");
        tracing::info!("NODEBOOTSTRAP_PROXY=none -- skipping nodeproxy");
        return Ok(());
    }
    let bin = binary_path(cfg, "nodeproxy");
    anyhow::ensure!(bin.exists(), "no nodeproxy binary at {} -- run `nodebootstrap fetch` first", bin.display());
    let kubeconfig = if cfg.worker_bootstrap_kubeconfig.is_some() {
        cfg.worker_nodelet_kubeconfig()
    } else {
        cfg.cluster_kubeconfig()?
    };
    let kubeconfig = kubeconfig.to_string_lossy().to_string();
    let ip_family = cfg.ip_family();
    let lb_method = std::env::var("NODEBOOTSTRAP_LB_METHOD").unwrap_or_else(|_| "round-robin".to_string());
    let binary = bin.to_string_lossy().to_string();
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodeproxy",
            description: "nodeproxy -- not-k8s Service routing (kube-proxy replacement)",
            exec_cmd: &bin.to_string_lossy(),
            after: (!cfg.worker).then_some("kube-apiserver.service"),
            env: &[
                ("KUBECONFIG", &kubeconfig),
                ("NODEPROXY_IP_FAMILY", &ip_family),
                ("NODEPROXY_LB_METHOD", &lb_method),
                ("NOTK8S_COMPONENT", "nodeproxy"),
                ("NOTK8S_COMPONENT_BINARY", binary.as_str()),
            ],
        },
    )
    .context("installing nodeproxy as a supervised service")
}

/// Remove only this host's control-plane services. The caller removes the
/// nodestore member first when this host is part of a replicated cluster.
/// Data and PKI remain on disk so an operator can recover or rejoin
/// deliberately.
pub fn remove_control_plane(cfg: &Config) {
    for name in ["kube-apiserver", "nodestore", "nodescheduler", "nodecontroller"] {
        service_mgr::remove(cfg, name);
    }
    tracing::info!("removed local control-plane services; retained control-plane data and PKI");
}

/// Wired into `run_all()` right after `targets::run_with` -- replaces
/// upstream `kube-scheduler` outright (see this module's doc comment).
/// Uses `kube-scheduler.kubeconfig`, not `admin.kubeconfig`: `pki.rs`
/// already minted the `system:kube-scheduler` identity for exactly this
/// purpose.
pub fn ensure_nodescheduler(cfg: &Config) -> Result<()> {
    let bin = binary_path(cfg, "nodescheduler");
    anyhow::ensure!(bin.exists(), "no nodescheduler binary at {} -- run `nodebootstrap fetch` first", bin.display());
    let kubeconfig = cfg.kubeconfig_dir().join("kube-scheduler.kubeconfig").to_string_lossy().to_string();
    let binary = bin.to_string_lossy().to_string();
    let forwarded = forwarded_env("NODESCHEDULER_");
    let mut env: Vec<(&str, &str)> = vec![
        ("KUBECONFIG", &kubeconfig),
        ("NOTK8S_COMPONENT", "nodescheduler"),
        ("NOTK8S_COMPONENT_BINARY", &binary),
    ];
    for (k, v) in &forwarded {
        env.push((k.as_str(), v.as_str()));
    }
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodescheduler",
            description: "nodescheduler -- not-k8s scheduler (kube-scheduler replacement)",
            exec_cmd: &bin.to_string_lossy(),
            after: Some("kube-apiserver.service"),
            env: &env,
        },
    )
    .context("installing nodescheduler as a supervised service")
}

/// Wired into `run_all()` right after `targets::run_with` -- replaces
/// upstream `kube-controller-manager` outright (see this module's doc
/// comment). Uses `kube-controller-manager.kubeconfig`, not
/// `admin.kubeconfig`: `pki.rs` already minted the `system:kube-
/// controller-manager` identity for exactly this purpose.
pub fn ensure_nodecontroller(cfg: &Config) -> Result<()> {
    let bin = binary_path(cfg, "nodecontroller");
    anyhow::ensure!(bin.exists(), "no nodecontroller binary at {} -- run `nodebootstrap fetch` first", bin.display());
    let kubeconfig = cfg.kubeconfig_dir().join("kube-controller-manager.kubeconfig").to_string_lossy().to_string();
    // nodecontroller's own certificatesigningrequest-signing-controller
    // otherwise searches a fixed list of well-known CA paths (k3s's, ...)
    // that don't include pki.rs's own -- point it at the real cluster CA
    // explicitly so CSR signing actually works, not just silently disabled
    // with a warning (nodecontroller's own load_signing_ca() behavior when
    // no candidate is found).
    let ca_cert = cfg.pki_dir().join("ca.crt").to_string_lossy().to_string();
    let ca_key = cfg.pki_dir().join("ca.key").to_string_lossy().to_string();
    let binary = bin.to_string_lossy().to_string();
    let forwarded = forwarded_env("NODECONTROLLER_");
    let mut env: Vec<(&str, &str)> = vec![
        ("KUBECONFIG", &kubeconfig),
        ("NODECONTROLLER_CSR_SIGNING_CA_CERT_PATH", &ca_cert),
        ("NODECONTROLLER_CSR_SIGNING_CA_KEY_PATH", &ca_key),
        ("NOTK8S_COMPONENT", "nodecontroller"),
        ("NOTK8S_COMPONENT_BINARY", &binary),
    ];
    for (k, v) in &forwarded {
        // Already set above with this crate's own CA paths -- an operator
        // overriding them via the environment isn't a case this needs to
        // support (nodebootstrap's own CA is always the correct one for a
        // cluster it just bootstrapped), so skip re-adding a duplicate
        // Environment= line rather than let a stray external
        // NODECONTROLLER_CSR_SIGNING_CA_*_PATH silently win.
        if k == "NODECONTROLLER_CSR_SIGNING_CA_CERT_PATH" || k == "NODECONTROLLER_CSR_SIGNING_CA_KEY_PATH" {
            continue;
        }
        env.push((k.as_str(), v.as_str()));
    }
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodecontroller",
            description: "nodecontroller -- not-k8s controller manager (kube-controller-manager replacement)",
            exec_cmd: &bin.to_string_lossy(),
            after: Some("kube-apiserver.service"),
            env: &env,
        },
    )
    .context("installing nodecontroller as a supervised service")
}
