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
//!
//! **Finding #4 (2026-08-23, found live in `release.yml`'s e2e against the
//! still-current k3s-based bootstrap path):** "already-existing" above was
//! an unverified assumption. Every one of nodecontroller's impersonated
//! writes 403'd -- `replicasets/status`, `endpointslices`, everything --
//! while pods ran fine underneath, so every status-reporting e2e test
//! (Deployment/ReplicaSet/DaemonSet/StatefulSet/CronJob/Job/PDB) timed out
//! instead of failing immediately. First hypothesis (wrong, but left as a
//! harmless belt-and-suspenders supplement below): that the `system:
//! controller:<name>` ClusterRoleBindings were themselves missing. Verified
//! live against a real deployed cluster instead of guessing further --
//! both the `system:controller:replicaset-controller` ClusterRole *and*
//! its binding to the `replicaset-controller` ServiceAccount were present
//! and correct. `kubectl auth can-i patch replicasets/status --as=system:
//! serviceaccount:kube-system:replicaset-controller` said `no`; `... can-i
//! update replicasets/status ...` said `yes`. **The real bootstrap policy
//! only ever grants `update` on `*/status` subresources (and on a few main
//! resources like `endpointslices`) for these narrowly-scoped controller
//! identities -- it never grants `patch` -- because real upstream
//! `kube-controller-manager` writes those with a full status `Update()`
//! call, never a JSON merge patch.** `nodecontroller` uses
//! `Patch::Merge`/`patch_status()` throughout instead, which is what every
//! one of these writes actually 403'd on. Rewriting every impersonated
//! write in `nodecontroller` to use `replace`/`replace_status()` instead
//! would match upstream's real verb usage most faithfully, but touches
//! ~15 call sites across nearly every controller with no way to compile
//! or test locally on this project's own constrained dev boxes -- too much
//! surface to get right blind. Fixed the lower-risk way instead, matching
//! this crate's own "supplement, don't duplicate" precedent (Finding #2):
//! grant exactly the `patch` verb, on exactly the resources/subresources
//! each controller's own code (`crates/nodecontroller/src/controllers/
//! *.rs`) is observed to `.patch()`/`.patch_status()`, on top of the real
//! role's own resource list -- `CONTROLLER_PATCH_GRANTS` below, one entry
//! per call site found. Widens each identity's verbs on resources it
//! already has other access to, not which resources it can touch at all,
//! so the per-controller blast-radius isolation Finding #3 introduced
//! impersonation for is unaffected.
//!
//! **Finding #5 (2026-08-23, release pipeline run 50):** the replacement
//! scheduler and controller-manager also use their base identities for the
//! shared informer reads that feed their controllers. The built-in
//! `system:kube-scheduler` role did not include the storage/CSI resources
//! (`persistentvolumeclaims`, `csinodes`, `csidrivers`, and friends), and
//! `system:kube-controller-manager` did not include the PV/PVC and storage
//! resources nodecontroller watches. DRA's three resources were already
//! supplemented for the scheduler by Finding #2, but the rest of the
//! scheduler's unconditional watch set was not. Against release run 50
//! this produced a storm of 403/410 reflector warnings, prevented CSI PVCs
//! from binding, and left DRA pods unschedulable. The narrowly-scoped read
//! supplements below grant only the exact shared watch inputs each binary
//! opens; they do not broaden either component's write permissions.

use anyhow::{Context, Result};

use crate::config::Config;

/// Every `system:serviceaccount:kube-system:<name>` identity
/// `impersonated_client()` (`crates/nodecontroller/src/lib.rs`'s
/// `upstream_controller_sa()`) can become. Single source of truth for both
/// the impersonate `Role` below and the `system:controller:<name>` binding
/// supplement (Finding #4) -- previously two hand-kept-in-sync lists, now
/// one.
const CONTROLLER_SA_NAMES: &[&str] = &[
    "node-controller",
    "service-account-controller",
    "namespace-controller",
    "endpointslice-controller",
    "resourcequota-controller",
    "replicaset-controller",
    "deployment-controller",
    "daemon-set-controller",
    "statefulset-controller",
    "generic-garbage-collector",
    "job-controller",
    "cronjob-controller",
    "ttl-after-finished-controller",
    "attachdetach-controller",
    "persistent-volume-binder",
    "pv-protection-controller",
    "root-ca-cert-publisher",
    "resource-claim-controller",
    "certificate-controller",
    "disruption-controller",
];

/// `(sa_name, apiGroup, resource)` -- Finding #4's `patch` verb supplement.
/// One entry per `.patch()`/`.patch_status()` call site found in
/// `crates/nodecontroller/src/controllers/*.rs`, traced to the exact
/// `Api<T>` (and, for `patch_status`, the `/status` subresource) each call
/// targets. Controllers with no entry here (`service-account-controller`,
/// `namespace-controller`, `generic-garbage-collector`, `ttl-after-
/// finished-controller`, `attachdetach-controller`) have no `.patch()`
/// call in their own file -- nothing to grant.
const CONTROLLER_PATCH_GRANTS: &[(&str, &str, &str)] = &[
    ("cronjob-controller", "batch", "cronjobs/status"),
    ("certificate-controller", "certificates.k8s.io", "certificatesigningrequests/status"),
    ("daemon-set-controller", "apps", "daemonsets/status"),
    ("endpointslice-controller", "discovery.k8s.io", "endpointslices"),
    ("disruption-controller", "policy", "poddisruptionbudgets/status"),
    ("node-controller", "", "nodes"),
    ("resource-claim-controller", "", "pods/status"),
    ("persistent-volume-binder", "", "persistentvolumes"),
    ("persistent-volume-binder", "", "persistentvolumes/status"),
    ("persistent-volume-binder", "", "persistentvolumeclaims"),
    ("persistent-volume-binder", "", "persistentvolumeclaims/status"),
    ("replicaset-controller", "apps", "replicasets/status"),
    ("deployment-controller", "apps", "replicasets"),
    ("deployment-controller", "apps", "deployments/status"),
    ("root-ca-cert-publisher", "", "configmaps"),
    ("job-controller", "batch", "jobs/status"),
    ("job-controller", "batch", "jobs"),
    ("resourcequota-controller", "", "resourcequotas/status"),
    ("statefulset-controller", "apps", "statefulsets/status"),
    ("pv-protection-controller", "", "persistentvolumes"),
    ("pv-protection-controller", "", "persistentvolumeclaims"),
];

/// Shared informer inputs read directly as `system:kube-scheduler` by
/// `crates/nodescheduler/src/watch.rs`. Upstream's scheduler has its own
/// complete bootstrap policy; this replacement has an intentionally
/// smaller, unconditional watch set and therefore needs this supplement.
const NODESCHEDULER_READ_GRANTS: &[(&str, &str)] = &[
    ("", "namespaces"),
    ("", "nodes"),
    ("", "pods"),
    ("", "services"),
    ("", "replicationcontrollers"),
    ("", "persistentvolumes"),
    ("", "persistentvolumeclaims"),
    ("apps", "replicasets"),
    ("apps", "statefulsets"),
    ("policy", "poddisruptionbudgets"),
    ("storage.k8s.io", "storageclasses"),
    ("storage.k8s.io", "csinodes"),
    ("storage.k8s.io", "csidrivers"),
    ("storage.k8s.io", "csistoragecapacities"),
    ("storage.k8s.io", "volumeattachments"),
    ("resource.k8s.io", "deviceclasses"),
    ("resource.k8s.io", "resourceclaims"),
    ("resource.k8s.io", "resourceslices"),
];

/// Shared informer inputs read directly as
/// `system:kube-controller-manager` by `crates/nodecontroller/src/watch.rs`.
/// Controller-specific writes still use the impersonated ServiceAccounts
/// and their existing narrow grants above.
const NODECONTROLLER_READ_GRANTS: &[(&str, &str)] = &[
    ("", "configmaps"),
    ("", "nodes"),
    ("", "resourcequotas"),
    ("", "services"),
    ("", "persistentvolumes"),
    ("", "persistentvolumeclaims"),
    ("apps", "deployments"),
    ("apps", "replicasets"),
    ("policy", "poddisruptionbudgets"),
    ("storage.k8s.io", "storageclasses"),
    ("storage.k8s.io", "volumeattachments"),
    ("resource.k8s.io", "resourceclaimtemplates"),
];

/// Supplements (does not replace) the built-in bootstrap roles -- see the
/// findings in this module's doc comment. Separate ClusterRoles/Bindings
/// rather than editing the built-in ones directly: those are reconciled by
/// kube-apiserver's own PostStartHook on every restart (this module's
/// first finding), so a hand-edit would just be overwritten.
fn supplemental_grants() -> String {
    let sa_resource_names: String =
        CONTROLLER_SA_NAMES.iter().map(|n| format!("    - {n}\n")).collect();
    let scheduler_read_rules: String = NODESCHEDULER_READ_GRANTS
        .iter()
        .map(|(group, resource)| {
            format!(
                "- apiGroups: [\"{group}\"]\n  resources: [\"{resource}\"]\n  verbs: [\"get\", \"list\", \"watch\"]\n"
            )
        })
        .collect();
    let controller_read_rules: String = NODECONTROLLER_READ_GRANTS
        .iter()
        .map(|(group, resource)| {
            format!(
                "- apiGroups: [\"{group}\"]\n  resources: [\"{resource}\"]\n  verbs: [\"get\", \"list\", \"watch\"]\n"
            )
        })
        .collect();
    // Finding #4: bind each impersonated SA to the real, already-existing
    // `system:controller:<name>` ClusterRole by name -- not re-deriving its
    // rules -- under a binding name of this module's own, so this applies
    // whether or not upstream's own `system:controller:<name>` binding is
    // actually present.
    let controller_bindings: String = CONTROLLER_SA_NAMES
        .iter()
        .map(|n| {
            format!(
                r#"---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: nodebootstrap:controller-sa-{n}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: system:controller:{n}
subjects:
- kind: ServiceAccount
  name: {n}
  namespace: kube-system
"#
            )
        })
        .collect();
    // Finding #4's actual fix: one ClusterRole+Binding per SA that appears
    // in CONTROLLER_PATCH_GRANTS, one rule per (apiGroup, resource) entry
    // for that SA.
    let patch_grants: String = CONTROLLER_SA_NAMES
        .iter()
        .filter_map(|sa| {
            let rules: String = CONTROLLER_PATCH_GRANTS
                .iter()
                .filter(|(entry_sa, _, _)| entry_sa == sa)
                .map(|(_, group, resource)| {
                    format!(
                        "- apiGroups: [\"{group}\"]\n  resources: [\"{resource}\"]\n  verbs: [\"patch\"]\n"
                    )
                })
                .collect();
            if rules.is_empty() {
                return None;
            }
            Some(format!(
                r#"---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: nodebootstrap:controller-patch-{sa}
rules:
{rules}---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: nodebootstrap:controller-patch-{sa}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: nodebootstrap:controller-patch-{sa}
subjects:
- kind: ServiceAccount
  name: {sa}
  namespace: kube-system
"#
            ))
        })
        .collect();
    format!(
        r#"
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: nodebootstrap:nodescheduler-dra
rules:
{scheduler_read_rules}---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: nodebootstrap:nodecontroller-watches
rules:
{controller_read_rules}---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: nodebootstrap:nodecontroller-watches
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: nodebootstrap:nodecontroller-watches
subjects:
- kind: User
  name: system:kube-controller-manager
  apiGroup: rbac.authorization.k8s.io
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
# authoritative list this must stay in sync with (CONTROLLER_SA_NAMES,
# this module's own single source of truth for that same list). A
# namespaced Role (not a ClusterRole) because every one of those SAs lives
# in kube-system -- resourceNames on a namespaced "serviceaccounts"
# resource only ever matches within the Role's own namespace, so this
# can't be tricked into impersonating a same-named SA elsewhere.
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
{sa_resource_names}---
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
{controller_bindings}{patch_grants}"#
    )
}

/// A handful of the ~90 bootstrap `system:` ClusterRoles that must exist if
/// the PostStartHook ran at all -- not the full list (that would just be
/// re-deriving upstream's own table, the exact duplication this module's
/// doc comment explains is unnecessary), just enough to catch "RBAC wasn't
/// actually enabled" or "the apiserver never became ready" with a clear
/// error instead of a mysterious later 403.
/// `system:controller:replicaset-controller` added by Finding #4: the
/// generic names below all existed even while every `system:controller:*`
/// ClusterRoleBinding this crate depends on for Finding #4 was silently
/// absent, so they alone don't catch that gap. This one is a stand-in for
/// the whole `system:controller:*` family this crate relies on.
const SENTINEL_CLUSTER_ROLES: &[&str] = &[
    "cluster-admin",
    "system:node",
    "system:discovery",
    "system:kube-scheduler",
    "system:controller:replicaset-controller",
];

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
        .write_all(supplemental_grants().as_bytes())
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
