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

## The end goal, stated precisely

**Full behavioural parity with every group below — nothing upstream does
gets silently dropped because it was inconvenient for this project's
architecture.** But this component's honest engineering problem is
different in kind from `nodelet`/`nodeproxy`/`nodescheduler`'s, and has to
be named plainly rather than papered over:

**This project's whole thesis is event-driven, zero-idle-poll design**
(CLAUDE.md's pitch; `SCHEDULER.md`'s own accounting of kube-scheduler's
three idle timers, two of which it eliminates outright). That thesis holds
for kubelet, kube-proxy, and kube-scheduler because almost everything they
do is *reacting to a state change someone told them about* — a Pod was
created, a Service's endpoints changed, a Node was labeled. A watch is
exactly the right primitive for "tell me when X changes."

**kube-controller-manager's core job is disproportionately the opposite
shape: noticing when something *failed to happen*.** A node that stops
heartbeating emits no event — silence is the signal, and silence cannot be
watched. A Job's `ttlSecondsAfterFinished` firing, a finished CSR aging
out, an HPA's external metric going stale — none of these have a source
event to subscribe to; the only way to know is to check the clock. This is
not a solvable-with-more-cleverness gap, it is what these controllers
*are*: deadline detectors. So unlike the other three components, **polling
here cannot be eliminated, only made efficient** — the honest goal is not
"zero timers," it's the same discipline `SCHEDULER.md` already applied to
its one irreducible timer (leader-election renewal), generalized to
everything in this crate that has the same shape.

### The mechanism: one shared deadline heap, not N tickers

`nodescheduler`'s backoff queue already proved the pattern this crate
reuses at larger scale (`SCHEDULER.md`'s table, `flushBackoffQCompleted`
row): *"A binary heap ordered by expiry plus one `tokio::time::sleep_until`
rearmed on push. Same semantics, zero idle wakeups."* `nodecontroller`
commits to exactly one instance of that primitive, shared across every
controller that needs deadline detection, rather than each controller
running its own `tokio::time::interval`:

- Every deadline any controller needs to notice — a node's next
  heartbeat-expiry check, a Job's TTL, a CSR's cleanup age, an HPA's next
  metrics poll — is a `(Instant, ControllerId, Key)` entry pushed into one
  min-heap.
- Exactly one task owns the heap and sleeps until the *single* nearest
  deadline (`sleep_until`, rearmed each time the heap's head changes),
  then dispatches to the owning controller's reconcile function and pops.
- **Cost is O(1) amortized per tick, not O(active controllers) or O(N
  objects) per tick** — idle cost is one sleeping task, not fourteen. This
  is the concrete, falsifiable version of "polling can't be avoided, but
  it can be made efficient": the number of *wakeups* stays proportional to
  the number of deadlines that actually elapse, never to how often you'd
  naively re-check.
- A controller registers a deadline once (on relevant watch events —
  e.g. a fresh node heartbeat pushes its next expiry check forward) and
  the heap does the rest; no controller owns its own clock.

### Which groups actually need this, and which don't

Most of the crate is genuinely event-only and gets nothing from the heap
at all — naming this explicitly is part of the discipline, so a future
change doesn't reach for a timer out of habit:

| Group | Polling need | Irreducible? | Mechanism |
|---|---|---|---|
| A: node-lifecycle (heartbeat→taint) | Yes — silence detection | Yes | heap: recheck at `lastHeartbeat + gracePeriod` |
| A: node-ipam | No | — | pure event (Node created/podCIDR empty) |
| B: service routing | No | — | pure event (Service/Pod/EndpointSlice watch) |
| C: identity/namespace | No | — | pure event |
| D: garbage-collector | Mostly no | Partially — safety-net relist | heap, long period (upstream: 30min), insurance against a missed/dropped watch event, not routine |
| D: resourcequota | Mostly no | Partially — usage resync | heap, coalesced, not per-quota-object |
| D: podgc | Yes — "has this terminated Pod aged out" | Yes | heap |
| E: workload controllers (RS/Deploy/DS/STS) | No | — | pure event |
| F: job/cronjob | Partially — CronJob's schedule itself is time-driven | Yes, for CronJob only | heap: one entry per CronJob, next fire time |
| F: ttl-after-finished | Yes | Yes | heap |
| G: attach-detach | Small — upstream's own reconciler loop is time-boxed (short period) as a consistency check against the runtime's actual state, not a discovery mechanism | Partially | heap, short period, mirrors upstream's own `reconcilerSyncLoopPeriod` |
| G: pv/pvc-protection, expansion, binder | No | — | pure event |
| H: DRA-adjacent | No | — | pure event |
| I: CSR signing/approving | No | — | pure event |
| I: CSR cleaner | Yes — age-based | Yes | heap |
| J: HPA | Yes — external metrics API is pull-only by construction, no push source exists | Yes, fully | heap: one entry per HPA, `syncPeriod` (upstream default 15s) |
| J: disruption (PDB) | Small — status recompute resync | Partially | heap, coalesced |

The heap-driven entries above are the *entire* polling surface of this
component — if a future controller implementation reaches for
`tokio::time::interval` outside this mechanism, that is the same kind of
regression `SCHEDULER.md` flags for its own timers: "if it ever fires,
that's a bug report, not a routine event" applies here too, just to a
different, larger set of cases where it's honestly expected to fire.

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
