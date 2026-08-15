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

### The mechanism: a CPU-budgeted governor over a wheel + a heap

`nodescheduler`'s backoff queue already proved the base pattern this
crate builds on (`SCHEDULER.md`'s table, `flushBackoffQCompleted` row):
*"A binary heap ordered by expiry plus one `tokio::time::sleep_until`
rearmed on push. Same semantics, zero idle wakeups."* That bounds wakeup
*count* — but not wakeup *cost*, and not the cost of a heap operation
under high churn. Two things this crate adds on top, because unlike the
scheduler's single backoff queue, this component's polling surface scales
with cluster/workload size and is rescheduled constantly (every node's
heartbeat renewal pushes its own expiry check forward, cluster-wide, once
per `node-monitor-period`):

**1. A hashed timing wheel for anything whose count scales with the
cluster**, in place of a heap for those specific cases. A `BinaryHeap` is
O(log n) insert/remove and pointer-chases on every sift — real cost once
n is "one entry per node" (or per terminated Pod, or per CSR) and those
entries reschedule constantly. The standard structure for exactly this
shape — many timers, high reschedule rate, bounded horizon — is a
**hashed timing wheel**: a fixed-size ring buffer of slots (a plain
array), each slot holding the entries due in that time bucket
(index/slab-linked, not a heap), with a cursor that advances one slot per
tick. Insert/cancel/reschedule is O(1) (compute the target slot, splice
in/out); tick advance is O(1) plus O(entries in that one slot); the array
is contiguous and cache-friendly where a heap's sift is pointer-chasing.
This is the same structure behind Linux kernel timers, Netty's
`HashedWheelTimer`, and Kafka's purgatory — not a novel invention here.
Horizon is sized to the longest relevant deadline in that wheel (node
grace period, upstream default 40s, plus margin); nothing in this crate
has a multi-hour deadline, so a small overflow heap for anything beyond
the horizon (the standard hierarchical-wheel escape hatch) is enough.
**A plain heap stays the right choice** for low-cardinality, one-entry-
per-object cases with no reschedule churn — CronJob schedules, HPA sync,
PDB/quota resync, ttl-after-finished, the GC safety-net relist. This is a
deliberate two-tier choice, not "wheel everywhere" — see the table below
for which structure each group actually uses.

**2. A hard CPU-time budget per tick, enforced by the same governor that
owns both structures.** Bounding wakeup count alone still allows a
thundering-herd burst (a control-plane restart re-arming every node's
check at once; a healed network partition un-tainting hundreds of nodes
in the same tick) to spike a core to 100% draining a large batch of
simultaneously-due entries in one pass. The fix is the same one real-time
game loops use for "more work became due this frame than the frame
budget allows": a **fixed tick period** (default 100ms) with a **CPU-time
budget per tick**, default target **0.3–1% of one core**
(`NODECONTROLLER_CPU_BUDGET_PERCENT`). Each tick drains due entries from
the wheel/heap while cumulative measured processing time stays under
budget; anything left over when the budget is hit waits for the next
tick rather than blocking the loop — bounded added latency under a
burst, in exchange for a hard CPU ceiling, which is the right trade for a
background daemon (unlike a renderer, where a missed frame is directly
user-visible — that distinction is *why* this transfers only partially:
see below). Both the wheel and the heap feed this one governor; it is
what actually caps the cost of processing what's due, not just how often
the process wakes up.

**What deliberately does not transfer from game-loop design**: spin-wait
hybrid timing for sub-millisecond frame-lock precision. That technique
exists in game/audio engines because visual/audio smoothness is the
literal product; nothing here has a perceptual deadline, and spin-waiting
would itself burn the CPU budget this mechanism exists to protect.
`sleep_until`'s ordinary precision (sub-ms to low-ms, whatever tokio's
own timer wheel gives for free) is already enough — noted explicitly so
a future contributor doesn't "improve" this into a spin-loop.

**3. Jitter on insert, so correlated deadlines don't bunch up in the
first place.** The budget governor caps the *cost* of a burst once it
happens; the cheaper fix is not creating the burst at all. Node
heartbeats are correlated by construction (every node renews on the same
period), so their expiry checks naturally land in near-identical wheel
slots; a batch of Pods created together (a Deployment scale-up) inherits
the same TTL and would later expire together too. On insert, add small
random jitter — a few percent of the deadline's own interval, the same
reason upstream jitters kubelet's own sync loop — so correlated entries
fan out across nearby slots instead of stacking into one. This is what
keeps *steady-state* load flat over time rather than sawtoothing every
`node-monitor-period`; the tick budget from (2) is then only the backstop
for the bursts jitter can't fully absorb (a genuine mass event, not
routine correlated renewal).

Implemented in `crates/nodecontroller/src/wheel.rs` (the timing wheel —
insert/cancel/advance as plain functions over a struct, no I/O, pure and
unit-tested standalone, same discipline `SCHEDULER.md` enforces for the
scheduling cycle) and `crates/nodecontroller/src/pacing.rs` (the tick/
budget governor — `Governor<K>` owns one wheel plus a deferred-overflow
queue; the low-cardinality heap variant for Groups D/F/G/I/J doesn't exist
yet, since nothing in the implemented Group A needs it — additive when a
heap-shaped controller actually lands). Falsifiable, not just asserted:
now that Group A (node-lifecycle, the wheel's first real consumer) has
landed, extend `deploy/measure.sh`'s per-second CPU sampling (already used
for the nodelet-vs-kubelet profiling system) to nodecontroller, checked
both at idle and under a synthetic thundering-herd e2e case — stop several
nodes' kubelets at once so their heartbeat expiries land in the same
tick, confirm CPU stays pinned near the configured budget instead of
spiking, and confirm the resulting taints still land within one tick
period of budget-driven delay rather than being unboundedly late.

### Which groups actually need this, and which don't

Most of the crate is genuinely event-only and gets nothing from either
structure — naming this explicitly is part of the discipline, so a
future change doesn't reach for a timer out of habit. "Mechanism" below
picks wheel vs. heap per the cardinality rule in the section above:

| Group | Polling need | Irreducible? | Mechanism |
|---|---|---|---|
| A: node-lifecycle (heartbeat→taint) | Yes — silence detection | Yes | **wheel** (one entry per Node, rescheduled every heartbeat): recheck at `lastHeartbeat + gracePeriod`, jittered |
| A: node-ipam | No | — | pure event (Node created/podCIDR empty) |
| B: service routing | No | — | pure event (Service/Pod/EndpointSlice watch) |
| C: identity/namespace | No | — | pure event |
| D: garbage-collector | Mostly no | Partially — safety-net relist | heap, long period (upstream: 30min), insurance against a missed/dropped watch event, not routine |
| D: resourcequota | Mostly no | Partially — usage resync | heap, coalesced, not per-quota-object |
| D: podgc | Yes — "has this terminated Pod aged out" | Yes | **wheel** (one entry per terminated Pod) |
| E: workload controllers (RS/Deploy/DS/STS) | No | — | pure event |
| F: job/cronjob | Partially — CronJob's schedule itself is time-driven | Yes, for CronJob only | heap: one entry per CronJob, next fire time (low cardinality, no reschedule churn) |
| F: ttl-after-finished | Yes | Yes | heap |
| G: attach-detach | Small — upstream's own reconciler loop is time-boxed (short period) as a consistency check against the runtime's actual state, not a discovery mechanism | Partially | heap, short period, mirrors upstream's own `reconcilerSyncLoopPeriod` |
| G: pv/pvc-protection, expansion, binder | No | — | pure event |
| H: DRA-adjacent | No | — | pure event |
| I: CSR signing/approving | No | — | pure event |
| I: CSR cleaner | Yes — age-based | Yes | **wheel** (one entry per CSR) |
| J: HPA | Yes — external metrics API is pull-only by construction, no push source exists | Yes, fully | heap: one entry per HPA, `syncPeriod` (upstream default 15s) |
| J: disruption (PDB) | Small — status recompute resync | Partially | heap, coalesced |

The wheel/heap entries above are the *entire* polling surface of this
component, all drained through the one CPU-budgeted governor — if a
future controller implementation reaches for `tokio::time::interval`
outside this mechanism, that is the same kind of regression
`SCHEDULER.md` flags for its own timers: "if it ever fires, that's a bug
report, not a routine event" applies here too, just to a different,
larger set of cases where it's honestly expected to fire.

## A. Node lifecycle — Tier 0 — **implemented** (taints only)

- `node-lifecycle-controller` (`crates/nodecontroller/src/controllers/node_lifecycle.rs`):
  taints a Node `NotReady`/`unreachable` after its heartbeat `Lease`
  (`kube-node-lease`, watched directly — not the heavier `NodeStatus`) goes
  stale past `NODECONTROLLER_NODE_MONITOR_GRACE_PERIOD_SECONDS` (default
  40s, upstream's own default). This is the wheel's first real consumer —
  one entry per Node, rescheduled on every renewal. **Not yet
  implemented**: pod eviction off a tainted Node (upstream's own
  rate-limited, per-zone process — a real design pass of its own, not a
  silent bolt-on) and flipping `status.conditions` to `Unknown` (a second
  writer racing nodelet's own status push). Both named explicitly in that
  file's own module doc, not silently dropped. `GAP_CLOSURE.md` explicitly
  scopes taint-after-missed-heartbeat to kube-controller-manager, not
  nodelet — nodelet only clears the one taint that is its own job
  (`node.cloudprovider.kubernetes.io/uninitialized`, `node.rs`).
- `node-ipam-controller` (`crates/nodecontroller/src/controllers/node_ipam.rs`):
  allocates `Node.spec.podCIDR(s)` out of `NODECONTROLLER_CLUSTER_CIDR`/
  `NODECONTROLLER_NODE_CIDR_MASK_SIZE` (defaults matching
  `deploy/setup-control-plane.sh`'s own `10.42.0.0/16`/`/24`). flannel is
  dead without this — `deploy/lib/cni.sh` already documents the dependency
  on `spec.podCIDR` being set. IPv4 single-stack only for now, matching
  this project's current CNI setup — additive, not a rework, to extend to
  dual-stack later.

e2e coverage: `deploy/lib/test/cases/node_lifecycle_controller.sh`. Opt in
with `deploy/bootstrap-source.sh --controller-manager=nodecontroller` (also
passes k3s's own `--disable-controller-manager`, the same two-halves-of-
one-switch pairing `--scheduler=nodescheduler` already established).

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
  ServiceAccount. **Confirmed live, the hard way**: `bootstrap-source.sh`'s
  own demo-pod smoke test fails to apply — consistently, not flakily —
  under `CONTROLLER_MANAGER=nodecontroller` (CI, `e2e.yml`), because
  without this controller a fresh namespace's `default` ServiceAccount
  never exists, and the apiserver's `ServiceAccount` admission plugin
  (loaded by default — confirmed in the same run's own log) rejects any
  pod that doesn't name one explicitly. This is the first real, felt
  consequence of Group C not existing yet, not a hypothetical.
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
