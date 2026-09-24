# nodemigrate full migration goal

Last updated: 2026-09-24

This document defines the complete intended scope and acceptance criteria for
the standalone `nodemigrate` utility. It is the task-specific authority for
this work. The linked status documents are living records and should be
updated as implementation, CI, and release evidence changes.

## Goal

Deliver a full, recoverable, bidirectional node migration utility among K3s,
upstream Kubernetes, and not-k8s (`nodestore`), including nodes that must join
an existing cluster to replace a member. Preserve workload behavior and
recoverable state through each transition. Treat CNI as an installation's
actual provider: Cilium is a required scenario, and migration must not assume
Flannel.

`nodemigrate` is a standalone release artifact. It must remain outside the
combined `notk8s` binary. Its release uses the exact version of the latest
regular not-k8s release, without changing or advancing the shared `VERSION`
for a nodemigrate-only release. The intended initial utility release is
coordinated with regular `v0.8.1`, carrying the component fixes tracked in
[NODEMIGRATE_BUGS.md](NODEMIGRATE_BUGS.md); nodemigrate must not independently
bump the regular release version. This is a target, not publication authority.

## Required migration behavior

- Detect and inspect supported local K3s, upstream Kubernetes, and nodestore
  installations on both control-plane and worker nodes, including service
  manager, API configuration, networking, datastore mode where available, and
  the state needed to select a valid migration path.
- Migrate K3s and upstream Kubernetes to nodestore, and nodestore back to a
  retained local K3s or upstream Kubernetes installation.
- Support a destination that joins an existing cluster, including replacement
  of a node that previously belonged to that cluster. Use the established
  nodebootstrap join/environment configuration and preserve its membership,
  CA, and cluster networking requirements.
- Support ordered cluster migration: transfer cluster API state at the
  control-plane stage, then replace control-plane and worker nodes against the
  joined destination without re-importing the same cluster-wide objects from
  every worker. Preserve all source Node scheduling metadata in the protected
  export so later nodes can complete after the source API loses quorum; use a
  private local copy of that export per node for node-specific volume and CNI
  recovery data.
- On upstream control-plane nodes, stop kubelet and its CRI static-pod
  sandboxes before starting the destination control plane, while retaining the
  source manifests for a recoverable return migration.
- For control-plane and worker replacement, use the destination API to detect
  and wait for node registration. If a same-name destination Node exists,
  require the operator to explicitly select its replacement; never treat its
  old Ready condition as proof that the new node joined. Preserve the source
  Node's labels, annotations, taints, and unschedulable setting on the new
  Node, and use an identity-preconditioned delete for any stale same-name Node.
- Discover and preserve the source CNI arrangement, including Cilium. Do not
  require Flannel, overwrite external CNI host configuration, or assume that
  copying API objects alone recreates host networking state.
- Transfer Kubernetes API resources, including custom resources, add-ons,
  secrets, and persistent volume metadata, while accounting for destination
  UIDs, controller-owned transient objects, and API compatibility. Live
  NodeMetrics and PodMetrics samples are collected again by their metrics
  provider and are not persistent migration state.
- Preserve persistent volume claims, bindings, topology, and payload access
  across migration for the provisioners configured in the source cluster. Copy
  node-local and hostPath payloads with their owning node and PV affinity;
  preserve provider configuration, credentials, and attachment behavior for
  CSI and network storage, using the provider's migration mechanism when one
  is required. Do not exclude a volume class by assumption. If a concrete
  provider prerequisite prevents a safe move, report the exact prerequisite
  and recovery state instead of silently dropping or rebinding the volume.
- Keep the source installation available but stopped/disabled by default.
  Only uninstall it when the operator explicitly requests that action.
- Protect exports and secrets, retain the export for recovery, restore the
  prior source service state when cutover has not succeeded, and report a
  usable recovery location and state when later stages fail.
- Make plan/inspection output actionable and ensure each operation reports
  its selected source, target, join mode, and consequential actions.
- Before an interactive migration, show a prominent warning about external
  backups and the high probability of data loss, then require the exact input
  `yes` to continue. Noninteractive migration commands write the same warning
  to their logs and continue without waiting for input.

## End-to-end acceptance scenarios

The dedicated migration exercise must have independent starting lanes for:

1. K3s with Flannel disabled and Cilium installed as the CNI.
2. Upstream Kubernetes (kubeadm) with Cilium installed as the CNI.

Each lane must install and verify upstream first, then install the hostPath
CSI driver and exercise both static hostPath PV/PVC and dynamically provisioned
CSI PV/PVC data. Install cert-manager, nginx, Traefik ingress, and additional
representative cluster programs/add-ons. Record source versions and CNI
configuration.

At each checkpoint—initial source, after migration to nodestore, and after
migration back—check node readiness, workload availability, ingress routing,
certificate readiness, add-on/custom-resource availability, PV/PVC binding,
and expected data in both volume paths. Capture diagnostics and exact artifact
versions on failure. The scenarios must include a separate existing-cluster
join/replacement case before full-join support can be called verified.

## Nodemigrate merge gate

Do not merge nodemigrate until both isolated runtime scenarios below pass:

1. A single-node K3s cluster with Cilium must migrate to not-k8s and back to
   K3s. The returned cluster must have no differences from the initial full
   migration-state checkpoint. Verify node identity and readiness, workloads,
   add-ons, ingress, certificates, persistent data, and Cilium state, and
   repeat the behavioral probes at each stage.
2. An upstream Kubernetes cluster with three control-plane nodes and two
   worker nodes must complete the same not-k8s round trip. Check membership and
   readiness for all five nodes, along with workloads, add-ons, ingress,
   certificates, persistent data, Cilium state, and the existing-cluster
   join/replacement path. Repeat the checks at each stage and compare the
   returned cluster with the initial checkpoint.

The scenarios may share one CI host when each cluster is isolated with QEMU or
another suitable mechanism so all five upstream nodes remain distinct while
running on that host. Docker is a candidate if the environment can fully
simulate the cluster behaviors required by these checks, including networking,
node identity, service management, storage, and node failure/isolation. A
passing container-only simulation is not evidence for behavior it does not
model. Record the
isolation method, topology, artifact versions, per-stage results, and
before/after state comparison in the CI status document. These are
nodemigrate-specific merge gates; the general build and e2e gates remain
excluded by the task-specific rules below.

## Build, test, and e2e rules for this objective

These task-specific rules override conflicting general build/e2e/test
instructions in `AGENTS.md` for nodemigrate work:

- Do not run the repository's general `build.yml` gate for this objective.
- Do not run the repository's general e2e gate for this objective.
- Do not run local Cargo builds, Cargo tests, or local e2e on the development
  host.
- For nodemigrate Rust changes, use the targeted `nodemigrate checks`
  workflow, which compiles and tests only the `nodemigrate` crate. If
  `nodebootstrap` or `nodestore` changes, use `quick-check.yml` with the
  corresponding `components=nodebootstrap` or `components=nodestore` input.
  Run the focused checks for every changed crate. These runs are allowed and
  are not a general build gate.
- The dedicated `nodemigrate-integration.yml` workflow is the migration
  runtime test. Its build step only prepares binaries for that test. It is
  manually dispatched and must not be run unless the user authorizes the
  migration runtime/e2e run. Record each lane and checkpoint independently.
- Static checks such as `bash -n`, formatting checks, and documentation/link
  review are allowed. Do not represent them as runtime migration evidence.
- A release workflow run is required only when carrying out the separately
  authorized publication. Release validation must use the latest regular
  release's version and must not advance shared `VERSION` for a standalone
  nodemigrate publication.

These rules alter only this nodemigrate objective; they do not amend the
repository-wide merge policy for other work.

## Living status documents

- [Status dashboard](NODEMIGRATE_STATUS.md) — current summary and next actions.
- [Migration implementation status](NODEMIGRATE_MIGRATION_STATUS.md) — path
  coverage, behavior evidence, and migration risks.
- [CI and integration status](NODEMIGRATE_CI_STATUS.md) — workflow design,
  permitted checks, run IDs, and per-lane runtime checkpoints.
- [Release status](NODEMIGRATE_RELEASE_STATUS.md) — version source, package
  separation, release readiness, and publication record.
