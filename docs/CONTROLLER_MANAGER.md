# nodecontroller — design

kube-controller-manager's job: every control loop in real Kubernetes that
isn't kubelet, kube-proxy, kube-scheduler, or etcd. This document is the
scope, grouped the way an operator actually reasons about "the controller
manager" rather than as upstream's flat 30-entry `--controllers` list —
each group below is one coherent piece of work with its own watches, its
own risk, and its own place in the delivery order. Source of truth for
membership is `kubernetes/kubernetes` `release-1.33`
`cmd/kube-controller-manager/app/controllermanager.go`'s
`NewControllerDescriptors()`, not the reference docs page (`SCHEDULER.md`
already found that page stale against source for kube-scheduler; assume
the same here until checked).

Status legend: **0** = Tier 0 (cluster non-functional without it), **1** =
Tier 1 (what most users mean by "controller manager"), **2** = Tier 2
(rounds out parity), **defer** = explicitly out of scope for this project
(no cloud provider) or low-value now (legacy token flows).

## A. Node lifecycle — Tier 0

- `node-lifecycle-controller`: taints a Node `NotReady`/`unreachable`
  after it misses heartbeats past `nodeMonitorGracePeriod` (upstream
  default 40s), evicts its pods after the eviction timeout. `GAP_CLOSURE.md`
  explicitly scopes this to kube-controller-manager, not nodelet — nodelet
  only clears the one taint that is its own job
  (`node.cloudprovider.kubernetes.io/uninitialized`, `node.rs`).
- `node-ipam-controller`: allocates `Node.spec.podCIDR(s)` out of
  `--cluster-cidr`/`--node-cidr-mask-size`. flannel is dead without this —
  `deploy/lib/cni.sh` already documents the dependency on
  `spec.podCIDR` being set.

Smallest blast radius of any group (no owner-ref graph, no cross-object
fan-out) — the plan's suggested first PR.

## B. Service routing — Tier 0

- `endpoints-controller` (legacy `Endpoints`) + `endpointslice-controller`
  + `endpointslice-mirroring-controller`: watches Service + Pod, produces
  EndpointSlices. `nodeproxy`'s `svc.rs` only *consumes* EndpointSlices —
  without this group, Services stop routing entirely the moment k3s's
  bundled copy is disabled. This is the one group where a regression is
  instantly, cluster-wide visible.

## C. Identity & namespace bootstrap — Tier 0

- `serviceaccount-controller`: ensures every namespace has a `default`
  ServiceAccount.
- `namespace-controller`: finalizer-driven namespace deletion (purges
  every namespaced object before the Namespace itself goes away).
- `root-ca-cert-publisher-controller`: writes the `kube-root-ca.crt`
  ConfigMap every namespace gets, that pods' projected SA tokens rely on
  to verify the apiserver.
- `clusterrole-aggregation-controller`: merges `aggregationRule`-selected
  ClusterRoles (the mechanism `view`/`edit`/`admin` are built from).

## D. Garbage collection & quota — Tier 0 / 2

- `garbage-collector-controller` (**0**): owner-reference cascade
  deletion. Without it `kubectl delete deployment` orphans ReplicaSets
  and Pods forever — this is arguably the single most-felt gap if
  skipped, since it's invisible until the first `kubectl delete` of
  anything with children.
- `resourcequota-controller` (**2**): enforces `ResourceQuota` objects.
- `podgc-controller` (**2**): reclaims terminated Pods past
  `--terminated-pod-gc-threshold`.

## E. Workload controllers — Tier 1

`replicaset-controller`, `deployment-controller`, `daemonset-controller`,
`statefulset-controller`. What most users mean by "the controller
manager." Each is a straightforward `Reconciler` instantiation over the
shared harness (see main plan) — the design risk here is proving the
harness abstraction pulls its weight across four fairly different
reconcile shapes (rolling update math differs completely between
Deployment/DaemonSet/StatefulSet), not any one of them individually.

## F. Batch controllers — Tier 1 / 2

- `job-controller`, `cronjob-controller` (**1**): straightforward,
  self-contained, no dependency on group E.
- `ttl-after-finished-controller` (**2**): sweeps finished Jobs past
  `spec.ttlSecondsAfterFinished`.

## G. Volume/storage lifecycle — Tier 0 / 2

- `attach-detach-controller` (**0**): nodelet already *consumes* its
  annotation (commit `9264e11`, "controller-managed attach-detach
  annotation") — volumes tests currently pass only because k3s's bundled
  copy does this today; disabling k3s's controller-manager without this
  reimplemented breaks every attach-requiring volume test that currently
  passes.
- `persistentvolume-binder-controller` (**2**): PVC↔PV binding for
  non-dynamic (pre-created PV) provisioning.
- `pv-protection-controller` / `pvc-protection-controller` (**2**):
  finalizer-based "don't delete a PV/PVC still in use."
- `persistentvolume-expander-controller` (**2**): PVC resize requests.

## H. DRA-adjacent — Tier 2

- `ephemeral-volume-controller` / `resourceclaim-controller`: creates
  `ResourceClaim`s from a Pod's `resourceClaimTemplates`. Pairs directly
  with nodelet's existing `runtime/cri/claims.rs`/`dra.rs` — nodelet
  already speaks the consumer side of this protocol via the raw-request
  `RawResourceClaim` escape hatch (k8s-openapi v1_33 pin, see CLAUDE.md).
- `device-taint-eviction-controller`: newer (1.31+), evicts pods off
  tainted devices.

## I. CSR / cluster PKI — Tier 2

- `certificatesigningrequest-signing-controller`,
  `-approving-controller`, `-cleaner-controller`.
- The **one** group with a real external dependency this plan can't
  design away: signing needs the cluster CA *key*
  (`server-ca.key`), confirmed as a live operational requirement by the
  (unmerged, profiling-only) `upstream-kube-apiserver-controller-manager`
  branch, which had to source it from k3s's own TLS dir. Document as a
  required permission/mount for this one controller in the eventual
  service unit, not something the rest of `nodecontroller` needs.
- `bootstrap-signer-controller`, `token-cleaner-controller` (**defer**):
  legacy kubeadm bootstrap-token flow, low value now that projected SA
  tokens are the default join mechanism.

## J. Autoscaling & disruption — Tier 2

- `horizontalpodautoscaler-controller`: needs a live metrics API
  (`metrics-server` or equivalent) to be useful at all — document as an
  external dependency this project installs for e2e (mirroring how
  `deploy/lib/e2e-full-setup.sh` already installs real CSI/DRA reference
  drivers for those gated tests) rather than something `nodecontroller`
  provides itself.
- `disruption-controller`: PodDisruptionBudget `.status` computation
  (`currentHealthy`, `disruptionsAllowed`), consumed by eviction API
  callers (`kubectl drain`, cluster-autoscaler-style tooling).

## Explicitly out of scope

`cloud-node-lifecycle-controller`, `service-controller` (cloud
LoadBalancer provisioning), `route-controller` — this project has no
cloud provider integration by design (see CLAUDE.md's "Confirmed
genuinely NOT kubelet's job" list, same reasoning applies one layer up).

## Delivery order

Group A first (smallest, validates the shared per-controller harness and
leader-election extraction with the lowest blast radius), then B and C
(both are "the cluster silently breaks without this" categories once
k3s's bundled copy is turned off), then D, then E/F, then G, then H/I/J.
Each group is its own PR per CLAUDE.md's merge protocol — build in CI,
never locally; new `deploy/lib/test/cases/*.sh` coverage per group (none
of this surface has *any* e2e coverage today — see the main design plan).
