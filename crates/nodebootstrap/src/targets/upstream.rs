//! Installs and runs real upstream `kube-apiserver` + `kube-controller-
//! manager` + `kube-scheduler` against `nodestore`, wired up with the PKI
//! this crate minted (not k3s's).
//!
//! Replaces `deploy/lib/upstream-kube-apiserver.sh` /
//! `upstream-kube-controller-manager.sh` / `upstream-kube-scheduler.sh` --
//! but where those scripts deliberately *borrow* k3s's already-generated
//! PKI (see `upstream-kube-apiserver.sh`'s header comment for why: nothing
//! else minted one), this target starts from `pki.rs`'s output instead, so
//! there is no k3s in the loop at all. `nodeproxy` already replaces
//! upstream `kube-proxy`; `nodelet` already replaces `kubelet` -- this is
//! only the last two-plus-one (apiserver/controller-manager/scheduler).
//!
//! `deploy/lib/upstream-kube-apiserver.sh`'s `detect_k8s_version()`
//! resolves the binary version off `k3s --version`; once k3s is gone that
//! has to become a pinned version this crate carries itself (matching
//! whatever `k8s-openapi` feature the workspace targets, e.g. `v1_34`).

use anyhow::Result;

use crate::config::Config;

pub fn run_with(_cfg: &Config) -> Result<()> {
    anyhow::bail!(
        "nodebootstrap::targets::upstream is a scaffold, not yet implemented -- see \
         docs/NODEBOOTSTRAP_PLAN.md Phase 1"
    )
}
