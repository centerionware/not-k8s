//! The seam that makes `docs/NODEBOOTSTRAP_PLAN.md` point 3 real: install
//! and start whichever apiserver/controller-manager/scheduler combination
//! `Config::target` names, using the PKI/kubeconfig `pki.rs`/
//! `kubeconfig.rs` already produced. Everything else in this crate
//! (`rbac.rs`, `service_reconciler.rs`, `manifests.rs`) only needs *a*
//! reachable, spec-compliant apiserver -- it doesn't know or care which one
//! this module started.
//!
//! `nodeapiserver` is the default target and runs this repository's
//! replacement against the shared PKI and datastore. `upstream` remains an
//! explicit compatibility/comparison target selected with
//! `--apiserver=upstream`.

pub mod nodeapiserver;
pub mod upstream;

use anyhow::Result;

use crate::config::{Config, Target};

pub fn run_with(cfg: &Config) -> Result<()> {
    match cfg.target {
        Target::Upstream => upstream::run_with(cfg),
        Target::NodeApiserver => nodeapiserver::run_with(cfg),
    }
}

/// Complete the target-specific post-nodelet handoff, if this target needs
/// one. Keeping the dispatch here preserves the target seam for the future
/// nodeapiserver implementation.
pub fn enable_nodelet_proxy(cfg: &Config) -> Result<()> {
    match cfg.target {
        Target::Upstream => upstream::enable_nodelet_proxy(cfg),
        Target::NodeApiserver => nodeapiserver::enable_nodelet_proxy(cfg),
    }
}

/// Complete the network-dependent part of the apiserver handoff. With
/// Flannel, wait for its bridge gateway; with an external CNI, publish the
/// selected non-loopback host address so in-cluster clients can reach the
/// local API Service.
pub fn refresh_network_advertise_address(cfg: &Config) -> Result<()> {
    match cfg.target {
        Target::Upstream => upstream::refresh_network_advertise_address(cfg),
        Target::NodeApiserver => nodeapiserver::refresh_network_advertise_address(cfg),
    }
}
