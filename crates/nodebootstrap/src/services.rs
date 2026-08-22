//! Installs this project's own components -- `nodestore`, `nodelet`,
//! `nodeproxy`, and (opt-in, not yet wired into `run_all`) `nodescheduler`/
//! `nodecontroller` -- as real, persistent, auto-restarting services via
//! `service_mgr.rs`. Replaces `deploy/lib/{nodestore,nodelet,nodeproxy,
//! nodescheduler,nodecontroller}-service.sh`.
//!
//! **Binary location**: every one of `fetch.rs`'s two paths
//! (`Source::Compile`/`Source::Release`) now stages its output at
//! `Config::toolchain_dir()/bin/<component-name>` -- one canonical
//! location this module looks in regardless of how the binary got there.
//!
//! **Credentials**: every component here uses the same `admin.kubeconfig`
//! `kubeconfig.rs` already writes -- matching current production behavior
//! exactly (`bootstrap-source.sh` points `nodelet`/`nodeproxy`/
//! `nodescheduler`/`nodecontroller` at the same admin/cluster-admin
//! kubeconfig today; there is no existing per-component RBAC restriction
//! to preserve or regress). Tightening this to per-component identities
//! (a real `system:node:<name>` cert for `nodelet` via `nodecontroller`'s
//! own CSR-signing flow, dedicated certs for the others) would be a new
//! improvement over current behavior, not something this port owes --
//! tracked as follow-up, not a gap introduced here.
//!
//! **`nodescheduler`/`nodecontroller` are not wired into `run_all`.**
//! `targets/upstream.rs` currently installs real upstream `kube-scheduler`/
//! `kube-controller-manager` unconditionally -- running this project's own
//! replacements *at the same time* would be two schedulers/controller-
//! managers racing over the same objects, exactly the failure `CLAUDE.md`
//! warns `components.sh`'s `want_nodescheduler`/`want_nodecontroller`
//! guards against. Wiring the swap (skip the upstream binary when
//! `NODEBOOTSTRAP_SCHEDULER=nodescheduler`/`NODEBOOTSTRAP_CONTROLLER_
//! MANAGER=nodecontroller` is set) is real follow-up work in
//! `targets/upstream.rs`, not this module -- `ensure_nodescheduler`/
//! `ensure_nodecontroller` are real and callable, just not auto-invoked
//! yet.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::service_mgr::{self, SupervisedService};

fn binary_path(cfg: &Config, name: &str) -> std::path::PathBuf {
    cfg.toolchain_dir().join("bin").join(name)
}

fn admin_kubeconfig(cfg: &Config) -> String {
    cfg.kubeconfig_dir().join("admin.kubeconfig").to_string_lossy().to_string()
}

/// The three components `run_all()` wires in by default, in the same
/// order it does (nodestore first -- see this module's doc comment on
/// ordering; nodelet/nodeproxy last). Does **not** include
/// `nodescheduler`/`nodecontroller` -- opt-in only, see this module's doc
/// comment.
pub fn run_with(cfg: &Config) -> Result<()> {
    ensure_nodestore(cfg)?;
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
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodestore",
            description: "nodestore -- not-k8s datastore (etcd v3 API over sqlite)",
            exec_cmd: &bin.to_string_lossy(),
            after: None, // nodestore is what other units order *after*, not the reverse
            env: &[("NODESTORE_LISTEN", &listen), ("NODESTORE_DATA_DIR", &data_dir)],
        },
    )
    .context("installing nodestore as a supervised service")
}

/// Not yet called from `run_all()` -- `nodelet` additionally needs
/// containerd/CNI (`containerd.rs`/`cni.rs`) and a reachable apiserver
/// (`targets::run_with`) to be meaningfully useful, so a caller wires this
/// in after those, same ordering `bootstrap-source.sh` uses today.
pub fn ensure_nodelet(cfg: &Config) -> Result<()> {
    let bin = binary_path(cfg, "nodelet");
    anyhow::ensure!(bin.exists(), "no nodelet binary at {} -- run `nodebootstrap fetch` first", bin.display());
    let kubeconfig = admin_kubeconfig(cfg);
    let runtime = std::env::var("NODELET_RUNTIME").unwrap_or_else(|_| "mock".to_string());
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodelet",
            description: "nodelet -- not-k8s node agent (kubelet replacement)",
            exec_cmd: &bin.to_string_lossy(),
            after: Some("kube-apiserver.service"),
            env: &[("KUBECONFIG", &kubeconfig), ("NODELET_RUNTIME", &runtime)],
        },
    )
    .context("installing nodelet as a supervised service")
}

/// Not yet called from `run_all()` -- see `ensure_nodelet`'s doc comment;
/// same ordering applies. Skipped entirely by a caller when
/// `NODEBOOTSTRAP_PROXY=none` (this project's Service routing may be
/// something else -- Cilium, a real kube-proxy -- or nothing).
pub fn ensure_nodeproxy(cfg: &Config) -> Result<()> {
    let bin = binary_path(cfg, "nodeproxy");
    anyhow::ensure!(bin.exists(), "no nodeproxy binary at {} -- run `nodebootstrap fetch` first", bin.display());
    let kubeconfig = admin_kubeconfig(cfg);
    let ip_family = cfg.ip_family();
    let lb_method = std::env::var("NODEBOOTSTRAP_LB_METHOD").unwrap_or_else(|_| "round-robin".to_string());
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodeproxy",
            description: "nodeproxy -- not-k8s Service routing (kube-proxy replacement)",
            exec_cmd: &bin.to_string_lossy(),
            after: Some("kube-apiserver.service"),
            env: &[("KUBECONFIG", &kubeconfig), ("NODEPROXY_IP_FAMILY", &ip_family), ("NODEPROXY_LB_METHOD", &lb_method)],
        },
    )
    .context("installing nodeproxy as a supervised service")
}

/// Real, callable, but **not** wired into `run_all()` -- see this module's
/// doc comment on why (races with `targets/upstream.rs`'s unconditional
/// upstream `kube-scheduler` until that module learns to skip it).
pub fn ensure_nodescheduler(cfg: &Config) -> Result<()> {
    let bin = binary_path(cfg, "nodescheduler");
    anyhow::ensure!(bin.exists(), "no nodescheduler binary at {} -- run `nodebootstrap fetch` first", bin.display());
    let kubeconfig = admin_kubeconfig(cfg);
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodescheduler",
            description: "nodescheduler -- not-k8s scheduler (kube-scheduler replacement)",
            exec_cmd: &bin.to_string_lossy(),
            after: Some("kube-apiserver.service"),
            env: &[("KUBECONFIG", &kubeconfig)],
        },
    )
    .context("installing nodescheduler as a supervised service")
}

/// Real, callable, but **not** wired into `run_all()` -- see this module's
/// doc comment (same reasoning as `ensure_nodescheduler`, against upstream
/// `kube-controller-manager`).
pub fn ensure_nodecontroller(cfg: &Config) -> Result<()> {
    let bin = binary_path(cfg, "nodecontroller");
    anyhow::ensure!(bin.exists(), "no nodecontroller binary at {} -- run `nodebootstrap fetch` first", bin.display());
    let kubeconfig = admin_kubeconfig(cfg);
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "nodecontroller",
            description: "nodecontroller -- not-k8s controller manager (kube-controller-manager replacement)",
            exec_cmd: &bin.to_string_lossy(),
            after: Some("kube-apiserver.service"),
            env: &[("KUBECONFIG", &kubeconfig)],
        },
    )
    .context("installing nodecontroller as a supervised service")
}
