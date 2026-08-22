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
//! applies either way). Supplements the built-in role with exactly the
//! three resources the error named, scoped to the `system:kube-scheduler`
//! identity `nodescheduler` already authenticates as -- not a broader
//! grant.
//!
//! **Finding #3 (2026-08-22, found live running `nodecontroller`), fixed
//! properly, not bridged:** real upstream `kube-controller-manager`
//! normally runs each of its control loops as its own narrowly-scoped
//! `system:serviceaccount:kube-system:<controller-name>` identity (via
//! `--use-service-account-credentials=true`, which `targets/upstream.rs`
//! sets) -- each with its own tightly-scoped `system:controller:<name>`
//! bootstrap ClusterRole. `nodecontroller` didn't do that impersonation
//! dance; every one of its controllers ran as the single blanket
//! `system:kube-controller-manager` identity, whose own built-in
//! bootstrap role was never meant to be broad enough for that (real
//! upstream barely uses it directly). Found live: `configmaps` creation
//! (the root-ca-cert-publisher controller) 403'd under that identity --
//! recorded as `docs/E2E_FINDINGS.md` finding 22.
//!
//! **Fixed in `nodecontroller` itself** (`crates/nodecontroller/src/
//! lib.rs`'s `impersonated_client`): it now builds one impersonated client
//! per controller, matching upstream's own identity-per-controller model.
//! This module's job shrinks accordingly, from "grant everything" to
//! granting exactly the `impersonate` verb the base identity needs to
//! *become* those narrower identities -- RBAC then authorizes each
//! controller's actual requests as whichever `system:serviceaccount:
//! kube-system:<name>` it impersonated, against that identity's own
//! already-existing `system:controller:<name>` bootstrap role. No
//! `cluster-admin` binding remains.

use anyhow::{Context, Result};

use crate::config::Config;

/// Supplements (does not replace) the built-in bootstrap roles -- see the
/// findings in this module's doc comment. Separate ClusterRoles/Bindings
/// rather than editing the built-in ones directly: those are reconciled by
/// kube-apiserver's own PostStartHook on every restart (this module's
/// first finding), so a hand-edit would just be overwritten.
const SUPPLEMENTAL_GRANTS: &str = r#"
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
---
# Lets system:kube-controller-manager become exactly the ServiceAccount
# identities nodecontroller's own impersonated_client() names -- see
# crates/nodecontroller/src/lib.rs's upstream_controller_sa() for the
# authoritative list this must stay in sync with. A namespaced Role (not a
# ClusterRole) because every one of those SAs lives in kube-system --
# resourceNames on a namespaced "serviceaccounts" resource only ever
# matches within the Role's own namespace, so this can't be tricked into
# impersonating a same-named SA elsewhere.
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: nodebootstrap:nodecontroller-impersonate-sa
  namespace: kube-system
rules:
- apiGroups: [""]
  resources: ["serviceaccounts"]
  verbs: ["impersonate"]
  resourceNames:
    - node-controller
    - service-account-controller
    - namespace-controller
    - endpointslice-controller
    - resourcequota-controller
    - replicaset-controller
    - deployment-controller
    - daemon-set-controller
    - statefulset-controller
    - generic-garbage-collector
    - job-controller
    - cronjob-controller
    - ttl-after-finished-controller
    - attachdetach-controller
    - persistent-volume-binder
    - pv-protection-controller
    - root-ca-cert-publisher
    - resource-claim-controller
    - certificate-controller
    - disruption-controller
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: nodebootstrap:nodecontroller-impersonate-sa
  namespace: kube-system
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: nodebootstrap:nodecontroller-impersonate-sa
subjects:
- kind: User
  name: system:kube-controller-manager
  apiGroup: rbac.authorization.k8s.io
---
# The two Impersonate-Group values impersonated_client() sends alongside
# every Impersonate-User -- groups are cluster-scoped, so this is a
# ClusterRole even though the SA impersonation above is namespaced.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: nodebootstrap:nodecontroller-impersonate-groups
rules:
- apiGroups: [""]
  resources: ["groups"]
  verbs: ["impersonate"]
  resourceNames:
    - system:serviceaccounts
    - system:serviceaccounts:kube-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: nodebootstrap:nodecontroller-impersonate-groups
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: nodebootstrap:nodecontroller-impersonate-groups
subjects:
- kind: User
  name: system:kube-controller-manager
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
    apply_supplemental_grants(&kubeconfig)
}

/// `kubectl apply -f -`, same subprocess-call posture `manifests.rs` uses
/// and explains.
fn apply_supplemental_grants(kubeconfig: &std::path::Path) -> Result<()> {
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
        .write_all(SUPPLEMENTAL_GRANTS.as_bytes())
        .context("writing the supplemental RBAC grants to kubectl's stdin")?;
    let status = child.wait().context("waiting for kubectl apply")?;
    anyhow::ensure!(status.success(), "kubectl apply -f - (supplemental RBAC grants) exited {status}");
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
