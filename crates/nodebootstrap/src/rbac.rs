//! The ~90 `system:` ClusterRoles/ClusterRoleBindings from upstream's
//! `bootstrappolicy` (`plugin/pkg/auth/authorizer/rbac/bootstrappolicy` in
//! `kubernetes/kubernetes`). Group O's RBAC half.
//!
//! **Finding (2026-08-22, verified against this project's own existing
//! deploy):** this module does not need to vendor or hand-build that list
//! at all. `deploy/setup-control-plane.sh` -- which has run a real
//! kube-apiserver (k3s's own embedded one) with `--authorization-mode`
//! including RBAC for as long as this project has existed -- contains zero
//! `kubectl create clusterrole`/`clusterrolebinding` calls, because
//! upstream `kube-apiserver` creates and reconciles the entire bootstrap
//! policy itself: a `PostStartHook` (`rbac/bootstrap-roles`, wired in
//! `pkg/controlplane` via `storage_rbac.go`'s `PostStartHook`) runs on
//! every apiserver start whenever `--authorization-mode` includes `RBAC`,
//! and it is a *reconciler* -- it re-applies the ~90 objects on every
//! restart, not a one-time install. k3s "bootstrapping RBAC for us" was
//! never k3s-specific behavior; it was this same PostStartHook running
//! inside k3s's embedded real apiserver process the whole time. Starting
//! real upstream `kube-apiserver` (`targets/upstream.rs`) with
//! `--authorization-mode=Node,RBAC` gets this for free, identically.
//!
//! What's left for this module to actually do: confirm the apiserver was
//! started with RBAC enabled and that the reconciler actually ran (a smoke
//! check, not a re-implementation), and apply anything genuinely specific
//! to *this* project that upstream's own bootstrap policy has no opinion
//! on -- e.g. any not-k8s-component-specific bindings that aren't already
//! covered by the built-in `system:kube-controller-manager`/
//! `system:kube-scheduler`/`system:node` identities `pki.rs` already issues
//! certs for.
//!
//! **Finding #2 (2026-08-22, found live running `nodescheduler` against
//! this exact stack):** `system:kube-scheduler`'s built-in bootstrap
//! ClusterRole on `v1.33.13` does **not** grant `get`/`list`/`watch` on
//! `resource.k8s.io`'s `deviceclasses`/`resourceslices`/`resourceclaims`
//! (DRA). `nodescheduler` watches those unconditionally on startup;
//! without this grant its reflector 403s in a tight retry loop and never
//! reaches the pod watch at all -- pods stay `Pending` forever with no
//! error surfaced anywhere near the actual cause. This is a real gap in
//! what this module verified is "enough" RBAC, not something upstream's
//! own `kube-scheduler` would hit the same way (unclear whether real
//! `kube-scheduler` gates its own DRA watches behind a feature check
//! `nodescheduler` doesn't have, or whether this grant lands in a later
//! k8s version's bootstrap policy -- not investigated further; the fix
//! applies either way). `apply_nodescheduler_dra_grant` supplements the
//! built-in role with exactly the three resources the error named, scoped
//! to the `system:kube-scheduler` identity `nodescheduler` already
//! authenticates as -- not a broader grant.

use anyhow::{Context, Result};

use crate::config::Config;

/// Supplements (does not replace) the built-in `system:kube-scheduler`
/// ClusterRole -- see the finding in this module's doc comment. A second
/// ClusterRole + Binding rather than editing the built-in one directly:
/// the built-in one is reconciled by kube-apiserver's own PostStartHook on
/// every restart (see this module's first finding), so a hand-edit to it
/// would just be overwritten.
const DRA_SCHEDULER_GRANT: &str = r#"
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: nodebootstrap:nodescheduler-dra
rules:
- apiGroups: ["resource.k8s.io"]
  resources: ["deviceclasses", "resourceclaims", "resourceslices"]
  verbs: ["get", "list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: nodebootstrap:nodescheduler-dra
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: nodebootstrap:nodescheduler-dra
subjects:
- kind: User
  name: system:kube-scheduler
  apiGroup: rbac.authorization.k8s.io
"#;

/// A handful of the ~90 bootstrap `system:` ClusterRoles that must exist if
/// the PostStartHook ran at all -- not the full list (that would just be
/// re-deriving upstream's own table, the exact duplication this module's
/// doc comment explains is unnecessary), just enough to catch "RBAC wasn't
/// actually enabled" or "the apiserver never became ready" with a clear
/// error instead of a mysterious later 403.
const SENTINEL_CLUSTER_ROLES: &[&str] =
    &["cluster-admin", "system:node", "system:discovery", "system:kube-scheduler"];

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_rbac {
        tracing::info!("skipping RBAC bootstrap verification (NODEBOOTSTRAP_SKIP_RBAC)");
        return Ok(());
    }
    let kubeconfig = cfg.kubeconfig_dir().join("admin.kubeconfig");
    verify_bootstrap_rbac(&kubeconfig)?;
    apply_nodescheduler_dra_grant(&kubeconfig)
}

/// `kubectl apply -f -`, same subprocess-call posture `manifests.rs` uses
/// and explains.
fn apply_nodescheduler_dra_grant(kubeconfig: &std::path::Path) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("kubectl")
        .args(["--kubeconfig", &kubeconfig.to_string_lossy(), "apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("spawning kubectl apply")?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(DRA_SCHEDULER_GRANT.as_bytes())
        .context("writing the DRA grant manifest to kubectl's stdin")?;
    let status = child.wait().context("waiting for kubectl apply")?;
    anyhow::ensure!(status.success(), "kubectl apply -f - (nodescheduler DRA RBAC grant) exited {status}");
    Ok(())
}

/// Shells out to `kubectl get clusterrole <name>` for each sentinel role
/// using the admin kubeconfig `kubeconfig.rs` wrote. A `kube` crate client
/// would be more idiomatic than shelling out, but every other install-time
/// check in this crate (`toolchain.rs`, `containerd.rs`) is already a
/// subprocess call, and pulling in the `kube`/`k8s-openapi`/tokio stack
/// into a one-shot CLI whose other checks are all synchronous subprocess
/// calls is not worth the async runtime it would drag in for one caller.
fn verify_bootstrap_rbac(kubeconfig: &std::path::Path) -> Result<()> {
    for role in SENTINEL_CLUSTER_ROLES {
        let status = std::process::Command::new("kubectl")
            .args(["--kubeconfig", &kubeconfig.to_string_lossy(), "get", "clusterrole", role])
            .output()
            .with_context(|| format!("running kubectl to check for ClusterRole {role}"))?;
        if !status.status.success() {
            anyhow::bail!(
                "bootstrap ClusterRole '{role}' is missing after the apiserver started -- either \
                 --authorization-mode didn't include RBAC, or the apiserver never reached ready. \
                 kubectl stderr: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        }
    }
    tracing::info!(
        checked = SENTINEL_CLUSTER_ROLES.len(),
        "bootstrap RBAC policy present (kube-apiserver's own PostStartHook, not vendored here -- \
         see this module's doc comment)"
    );
    Ok(())
}
