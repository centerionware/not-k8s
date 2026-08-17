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

### The mechanism: informer caches, keyed queues, and a small timer wheel

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

**2. Event controllers do not use a process-wide CPU or request-rate
governor.** Each informer updates a local cache and enqueues an object key
into a deduplicating `KeyedWorkQueue`; a bounded worker reconciles the latest
cached object. A burst therefore produces at most one pending key and one
reconcile per controller at a time, while unrelated controllers remain
independent. Network waits are ordinary async waits, and writes are not
delayed by an arbitrary global sleep. This is the same cache/lister →
workqueue → worker boundary used by upstream client-go.

The event workers remain fully asynchronous: waiting on the apiserver does
not block a Tokio worker. The queue is the backpressure boundary; it is not a
sleep loop and it does not impose a process-wide CPU target.

**3. Jitter on insert, so correlated deadlines don't bunch up in the
first place.** Node
heartbeats are correlated by construction (every node renews on the same
period), so their expiry checks naturally land in near-identical wheel
slots; a batch of Pods created together (a Deployment scale-up) inherits
the same TTL and would later expire together too. On insert, add small
random jitter — a few percent of the deadline's own interval, the same
reason upstream jitters kubelet's own sync loop — so correlated entries
fan out across nearby slots instead of stacking into one. This is what
keeps *steady-state* load flat over time rather than sawtoothing every
`node-monitor-period`; the wheel is only the backstop for genuinely
time-driven silence detection, not a polling mechanism for object state.

### Shared informer snapshots and API admission

The event-driven half has a different failure mode from timer work. A
controller can be entirely async and still overload a small apiserver by
starting many requests at once, or by reconciling every copy of a burst
without coalescing it. The implementation therefore treats the shared
watchers as informer-like caches: their `InitApply` sequence is the startup
snapshot, and controllers do not issue a second seed `LIST` for the same
resource. One underlying watch is shared by every controller that consumes a
given built-in kind, including garbage collection where the typed watch is
available.

Controller traffic uses an ordinary async client. There is no process-wide
request-rate limiter: queue deduplication and bounded workers control
reconcile fan-out without adding latency to unrelated controllers. Leader
election still uses a separate client so lease renewal cannot be coupled to a
controller's cache or reconcile state.

Initial informer snapshots have a second, independent admission limit:
`NODECONTROLLER_WATCH_STARTUP_CONCURRENCY` defaults to 2. A shared watch holds
one permit only until its `InitDone` snapshot boundary, then returns it while
the live watch remains active. Garbage collection admits its dynamically
discovered kinds at one stream per second for the same reason. This limits the
expensive startup work (and its response fan-out) without disabling a
controller domain or imposing a permanent cap on required steady-state
watches.

All event-driven controllers now use `workqueue::KeyedWorkQueue`: watch
handlers update the cache and enqueue a key, while one async worker
reconciles the latest cached object. This removes duplicate GET/patch cycles
from a burst without blocking a Tokio worker. The remaining timers are the
small node-lifecycle timing wheel and controllers whose semantics require a
clock, such as CronJob scheduling and terminal-object cleanup.

Implemented in `crates/nodecontroller/src/watch.rs`,
`crates/nodecontroller/src/workqueue.rs`, and
`crates/nodecontroller/src/wheel.rs`. The watch layer shares informer-like
snapshots, the queue coalesces keys, and the wheel handles only silence
deadlines. Startup admission remains bounded because initial LIST fan-out is
an independent concern from steady-state event processing.

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
| D: resourcequota | No | — | informer cache + keyed queue |
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
| J: disruption (PDB) | No | — | informer cache + keyed queue |

The wheel/heap entries above are the *entire* polling surface of this
component; event controllers must not reach for `tokio::time::interval`
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

## B. Service routing — Tier 0 — **endpointslice-controller implemented**

- `endpointslice-controller` (`crates/nodecontroller/src/controllers/endpoint_slice.rs`):
  watches Service + Pod, produces `discovery.k8s.io/v1` EndpointSlices.
  `nodeproxy`'s `svc.rs` only *consumes* EndpointSlices — without this,
  Services stop routing entirely the moment k3s's bundled copy is
  disabled, the one group where a regression is instantly, cluster-wide
  visible. One EndpointSlice per Service (not upstream's size-limited,
  multi-slice-per-Service scheme — additive if ever needed at real scale),
  server-side-applied so repeated reconciles are idempotent. **Not
  implemented**: `endpoints-controller` (the legacy `v1.Endpoints`
  object — nothing in this project reads it) and
  `endpointslice-mirroring-controller` (mirrors a hand-written `Endpoints`
  object — a niche path). Both named explicitly in that file's own module
  doc. Owner-reference cascade delete also isn't relied on yet (Group D's
  `garbage-collector-controller` doesn't exist) — a Service delete
  explicitly deletes its own EndpointSlice instead.

## C. Identity & namespace bootstrap — Tier 0 — **serviceaccount-controller and root-ca-cert-publisher-controller implemented**

- `serviceaccount-controller` (`crates/nodecontroller/src/controllers/service_account.rs`):
  ensures every namespace has a `default` ServiceAccount. **Landed earlier
  than the rest of Group C, and not by plan** — confirmed live, the hard
  way, twice: `bootstrap-source.sh`'s own demo-pod smoke test, and
  separately `deploy/lib/test/harness.sh`'s own namespace-setup preflight,
  both failed hard under `CONTROLLER_MANAGER=nodecontroller` (CI,
  `e2e.yml`), because without this controller a fresh namespace's
  `default` ServiceAccount never exists, and the apiserver's
  `ServiceAccount` admission plugin (loaded by default — confirmed in the
  same run's own log) rejects any pod that doesn't name one explicitly.
  Without this piece, Group A was un-testable, not just incomplete — so
  this one controller (pure event, watch Namespace, create-if-missing) was
  pulled forward. The rest of Group C below is still not implemented.
- `namespace-controller`: finalizer-driven namespace deletion (purges
  every namespaced object before the Namespace itself goes away).
- `root-ca-cert-publisher-controller`
  (`crates/nodecontroller/src/controllers/root_ca_publisher.rs`,
  **implemented, pulled forward like `serviceaccount-controller` was**):
  writes the `kube-root-ca.crt` ConfigMap every namespace gets, that a
  Pod's default projected service-account-token volume mounts alongside
  the token so anything inside the Pod building an in-cluster client can
  verify the apiserver's TLS certificate. Found load-bearing the same way
  `serviceaccount-controller` was — live in CI, not from reading the spec:
  while verifying Group G, the real CSI `external-provisioner` sidecar
  (which builds its own in-cluster client, same as any properly-written
  controller) logged `"Expected to load root CA config from
  /var/run/secrets/.../ca.crt ... no such file or directory"` and never
  became a working client, so `csi_pvc.sh`/`csi_attach.sh`'s PVCs sat
  `Pending` forever with zero provisioning events. This was one real cause,
  but not the final Group G blocker: after it was fixed, the provisioner
  became a healthy client and exposed the binder handoff bug documented
  below. Reads the CA once at startup from nodecontroller's own ambient
  kubeconfig (matching this crate's existing "don't solve cert bootstrap,
  just read `$KUBECONFIG`" stance) rather than a `--root-ca-file` flag;
  named gaps: no CA rotation support (needs a restart to pick up a
  rotated CA) and no protection against a hand-deleted ConfigMap (it just
  gets recreated on the next Namespace reconcile, not blocked).
- `clusterrole-aggregation-controller`: merges `aggregationRule`-selected
  ClusterRoles (the mechanism `view`/`edit`/`admin` are built from).

## D. Garbage collection & quota — Tier 0 / 2 — **garbage-collector-controller and resourcequota-controller implemented** (object-count quota only)

- `garbage-collector-controller` (`crates/nodecontroller/src/controllers/garbage_collector.rs`,
  **implemented, generic across every namespaced kind via dynamic
  discovery**): owner-reference cascade deletion. Landed right after Group
  E, as planned — Group E is what first gives it a real owner chain
  (Deployment→ReplicaSet→Pod) worth cleaning up. Built via `kube::discovery`
  rather than a hardcoded kind list: one dynamic watch per discovered
  namespaced/watchable/deletable resource kind, funneled into a single
  event loop that tracks live UIDs and a reverse owner→children index
  purely from watch events — recursion (grandchild cleanup) falls out of
  the event loop itself (a cascade-deleted child's own Delete event
  re-enters the same loop), no explicit recursive graph walk. Real
  simplifications, named in that file's own module doc: (1) discovery runs
  once at startup, not on a live/invalidatable RESTMapper — a CRD installed
  after nodecontroller starts is invisible to it until restarted; (2)
  namespaced resources only — matches upstream's actual scope, since
  `OwnerReference` carries no namespace field and cross-namespace ownership
  isn't representable at all; (3) background propagation only —
  `Foreground`/`Orphan` `propagationPolicy` requests are not honored, every
  delete cascades immediately regardless of what the caller asked for; (4)
  `coordination.k8s.io` (Lease) and `events.k8s.io`/`Event` are excluded
  from discovery — high-churn, GC-irrelevant kinds.
- `resourcequota-controller` (`crates/nodecontroller/src/controllers/resource_quota.rs`,
  **implemented, object-count quotas only**): keeps `ResourceQuota.status.used`
  current for `pods` and `services` counts. Worth stating precisely: the
  *enforcement* half of ResourceQuota (rejecting an over-quota create) is
  the apiserver's own `ResourceQuota` admission plugin, not
  kube-controller-manager's job at all — that already works today,
  unmodified, regardless of which controller-manager runs. This
  controller only maintains the status a human reads. Compute-resource
  quotas (`requests.cpu`, `limits.memory`, ...) need real `Quantity`
  arithmetic — `k8s_openapi`'s `Quantity` is a bare string newtype with no
  arithmetic at all — and are deferred as genuinely separate work, not a
  gap in this file. Every unsupported `spec.hard` key is left out of
  `status.used` entirely rather than guessed at.
- `podgc-controller` (**2**): reclaims terminated Pods past
  `--terminated-pod-gc-threshold`. Not implemented.

## E. Workload controllers — Tier 1 — **all four implemented (replicaset, deployment, daemonset, statefulset)**

- `replicaset-controller` (`crates/nodecontroller/src/controllers/replica_set.rs`,
  **implemented**): ensures a ReplicaSet's `spec.replicas` Pods exist,
  matching its selector and pod template. The foundation the rest of this
  group is built on — `deployment-controller` manages ReplicaSets, not
  Pods directly, so nothing else here can produce a real Pod without this
  landing first. Real simplifications, all named in that file's own
  module doc rather than silently dropped: no adoption of pre-existing
  unowned Pods (only manages Pods it created itself), a simplified
  scale-down ranking (not-Ready first, then by name — not upstream's full
  multi-criteria ranking), `status.availableReplicas` mirrors
  `readyReplicas` (`minReadySeconds` isn't tracked), and — same as every
  other object this crate creates before `garbage-collector-controller`
  exists — a deleted ReplicaSet's Pods aren't cleaned up automatically yet.
- `deployment-controller` (`crates/nodecontroller/src/controllers/deployment.rs`,
  **implemented**): manages ReplicaSets, not Pods directly — one owned
  ReplicaSet per distinct Pod template ("revision", identified by a
  `pod-template-hash` label), rolling-update surge/unavailable budgets (or
  `Recreate`) to shift replica counts from old revisions to the new one,
  `revisionHistoryLimit`-bounded cleanup of fully-drained old ReplicaSets.
  Real, named simplifications (see that file's own module doc for the
  full reasoning): its own internally-consistent template hash rather than
  upstream's specific FNV algorithm (nothing compares the two); no
  hash-collision-count retry; old-ReplicaSet scale-down always drains
  oldest-first rather than upstream's "most unhealthy first"; `Recreate`
  waits on ReplicaSet `status.replicas` reaching 0, not real per-Pod
  termination; no rollback, no revision-history annotations, no
  `DeploymentCondition`/`Progressing` tracking — `status.replicas`/
  `updatedReplicas`/`readyReplicas`/`availableReplicas` are kept current,
  the human-readable condition list is not populated.
- `daemonset-controller` (`crates/nodecontroller/src/controllers/daemon_set.rs`,
  **implemented**): places one Pod per eligible Node directly (`spec.nodeName`
  set at creation, bypassing `nodescheduler` — the same scheduler-bypass
  upstream's own DaemonSet controller uses), no ReplicaSet involved. Real,
  named simplifications (see that file's own module doc): node eligibility
  is nodeSelector + taint/toleration only (no node/pod affinity
  evaluation); no implicit built-in-taint tolerations (a DaemonSet's own
  template must name every taint it needs to tolerate, same as it would
  need to against upstream in practice); rolling update is a flat
  per-reconcile `maxUnavailable` delete-then-create budget (no `maxSurge`
  create-before-delete, no `OnDelete` strategy); no `ControllerRevision`
  history.
- `statefulset-controller` (`crates/nodecontroller/src/controllers/stateful_set.rs`,
  **implemented**): stable-identity Pods (`{name}-0`, `{name}-1`, ...)
  created/deleted/updated directly, no ReplicaSet involved. Real, named
  simplifications (see that file's own module doc): `podManagementPolicy:
  OrderedReady` (the default) and `Parallel` are both implemented;
  rolling update is always sequential one-ordinal-at-a-time (the alpha
  `MaxUnavailableStatefulSet` feature gate's `maxUnavailable` field isn't
  honored, `partition` is); `volumeClaimTemplates` PVCs are created (never
  deleted — matches upstream's *default* `Retain` policy, but the
  `Delete` retention policy is unimplemented so a StatefulSet that asks
  for it won't get it); no PVC-bound readiness gate (Pod creation doesn't
  wait for `Bound`, since PV binding itself is Group G, not implemented);
  no `ControllerRevision` history/rollback.

Group E is now feature-complete at this project's documented scope: all
four workload controllers real users mean by "the controller manager"
are implemented and e2e-verified.

## F. Batch controllers — Tier 1 / 2 — **all three implemented (job, cronjob, ttl-after-finished)**

- `job-controller` (`crates/nodecontroller/src/controllers/job.rs`,
  **implemented**): runs a Job's Pods to completion — creates up to
  `spec.parallelism` Pods, reports `Complete`/`Failed` once the target is
  reached or `backoffLimit` is exceeded. Purely event-driven, same shape as
  `replicaset-controller`. Real simplifications, named in that file's own
  module doc: `NonIndexed` completion mode only (no per-index tracking); no
  `podFailurePolicy`/`successPolicy`; no `activeDeadlineSeconds` (needs a
  poll timer this controller deliberately doesn't have); completion/failure
  counts recomputed from the live Pod set each reconcile rather than
  upstream's `uncountedTerminatedPods` bookkeeping (equivalent here because
  this controller never deletes a terminal Pod itself); `spec.managedBy`
  honored as a skip.
- `cronjob-controller` (`crates/nodecontroller/src/controllers/cron_job.rs`,
  **implemented**): creates a Job from `spec.jobTemplate` each time
  `spec.schedule` comes due. The one genuinely poll-driven controller in
  this group (a periodic scan, the "plain heap tier" this doc's mechanism
  section describes — one entry per CronJob, not per node, so no
  `wheel.rs` involvement). Schedule parsing/next-run math is a small
  from-scratch 5-field cron parser, `crates/nodecontroller/src/cron_schedule.rs`
  (same "write it, don't pull in a dependency for a small well-scoped
  surface" call this crate already made for FNV template hashing). Real
  simplifications: catches up one missed schedule boundary per tick, not
  every boundary missed during downtime; `startingDeadlineSeconds` skips a
  too-late boundary rather than tracking each missed occurrence
  individually; `spec.timeZone` is not honored — every schedule evaluates
  in UTC; `concurrencyPolicy` (`Allow`/`Forbid`/`Replace`) is fully
  implemented.
- `ttl-after-finished-controller`
  (`crates/nodecontroller/src/controllers/ttl_after_finished.rs`,
  **implemented**): deletes a finished Job once
  `spec.ttlSecondsAfterFinished` has elapsed since it finished (deletion
  cascades to the Job's Pods via `garbage-collector-controller`, not
  handled here directly). Also poll-driven, also the plain-heap tier — a
  flat periodic scan of the cached Job set rather than a per-Job wheel/heap
  entry, since expected cardinality (finished Jobs with a TTL set,
  cluster-wide) doesn't justify the extra structure.

## G. Volume/storage lifecycle — Tier 0 / 2 — **implemented (attach-detach, binder, pv/pvc-protection; expander scoped out) — static and dynamic paths e2e-verified**

- `attach-detach-controller` (`crates/nodecontroller/src/controllers/attach_detach.rs`,
  **implemented**): creates/deletes `VolumeAttachment` objects so a CSI
  driver's external-attacher sidecar actually attaches a volume — nodelet
  already *consumes* the result (commit `9264e11`, "controller-managed
  attach-detach annotation"; nodelet itself sets the
  `volumes.kubernetes.io/controller-managed-attach-detach` Node annotation
  this controller's existence is predicated on). Desired state (`(driver,
  PV, node)` triples wanted by live, scheduled Pods) is recomputed fresh
  from the Pod/PVC/PV caches on every relevant event rather than tracked
  incrementally — real simplifications named in the file's own module doc:
  CSI volumes only (no in-tree plugin types — this project has no cloud
  provider to migrate from anyway), a Pod keeps its volumes desired until
  it's fully removed from the apiserver (not just `deletionTimestamp` set,
  deliberately conservative), and no `--node-detach-timeout`-style
  force-detach for a node that goes permanently unreachable.
- `persistentvolume-binder-controller`
  (`crates/nodecontroller/src/controllers/pv_binder.rs`, **implemented;
  static and dynamic paths e2e-verified**): binds a PVC
  to a PV and owns the upstream-compatible handoff to dynamic provisioners.
  Static matching runs first (hand-created PV, matched by storage class +
  access modes). When no PV matches, the controller resolves the
  StorageClass and writes
  `volume.kubernetes.io/storage-provisioner=<class.provisioner>`; the
  external provisioner deliberately ignores the claim until this annotation
  exists. For `WaitForFirstConsumer`, that write waits for the scheduler's
  `volume.kubernetes.io/selected-node` annotation. Once the provisioner
  creates a PV with `spec.claimRef`, this controller writes the PV/PVC binding
  state and publishes `pv.kubernetes.io/bind-completed` last. That annotation
  is a real synchronization barrier: kube-scheduler's VolumeBinding plugin
  classifies an Immediate claim as unbound without it even when
  `spec.volumeName` and `status.phase=Bound` are already present.
  Load-bearing in practice
  despite the plan's original Tier 2 label: this project's own `csi_pvc.sh`/
  `csi_attach.sh` e2e tests gate on `PVC.status.phase == Bound`, which
  nothing sets without this controller once k3s's own copy is disabled.
  Named simplification: no capacity comparison for static matching
  (`Quantity` has no arithmetic — the same gap `resourcequota-controller`
  documents), and no unbind/reclaim-policy handling once bound.
  **Issue #30 diagnosis**: after the root-CA, nodeproxy, datastore, watch
  pressure, and informer/workqueue hypotheses were isolated, the decisive
  run showed a healthy CSI provisioner with populated caches, but no PVC
  event, no provisioning event, and no `CreateVolume`. Comparing the Rust
  binder to Kubernetes 1.33's `provisionClaimOperationExternal` found the
  missing transition above. The previous implementation returned when no PV
  existed, which is exactly where upstream annotates the PVC to wake the
  external provisioner. The first post-fix CSI run then exposed the second
  missing upstream transition: provisioning and PVC phase binding succeeded,
  but kube-scheduler rejected the claim until the binder also published
  `bind-completed`. Driver-independent e2e assertions now check both contracts
  directly in `storage_lifecycle_controllers.sh`; the existing real
  `csi_pvc.sh`/`csi_attach.sh` tests remain the end-to-end gate. Exact-head
  targeted e2e run 31998373256 passed dynamic provisioning, filesystem PVC,
  raw-block PVC, and attach-required VolumeAttachment on 2026-08-17.
- `pv-protection-controller` / `pvc-protection-controller`
  (`crates/nodecontroller/src/controllers/storage_protection.rs`,
  **implemented**): the standard finalizer-based "don't let this disappear
  while something still needs it" pattern — PV in use means its
  `spec.claimRef` still points at a PVC that actually exists (**not**
  `status.phase == "Bound"`, a bug caught in review before it shipped:
  this slice never transitions a PV's phase back to `Released` after its
  PVC is deleted, so phase-based protection would leave every bound PV
  permanently undeletable), PVC in use means referenced by name from a
  live Pod's `spec.volumes`. No admission-time delete rejection (this
  controller only manages the finalizer itself, not a second enforcement
  layer) — the protection still holds since the object won't actually
  disappear until the finalizer is removed.
- `persistentvolume-expander-controller` (**scoped out**): this project has
  no in-tree volume plugins to expand (this controller's real upstream job
  is in-tree `ControllerExpandVolume` calls), and CSI resize is handled
  directly by the external-resizer sidecar watching PVCs — no
  controller-manager involvement needed for the CSI-only case this project
  targets. Not a gap, the same reasoning `CLAUDE.md`'s "confirmed genuinely
  NOT kubelet's job" list uses one layer up.

## H. DRA-adjacent — Tier 2 — **ephemeral-volume-controller implemented; device-taint-eviction-controller scoped out**

- `ephemeral-volume-controller` / `resourceclaim-controller`
  (`crates/nodecontroller/src/controllers/resource_claim.rs`,
  **implemented**): creates a `ResourceClaim` from a Pod's
  `spec.resourceClaims[].resourceClaimTemplateName` entries and records the
  generated name in `pod.status.resourceClaimStatuses`. Pairs directly with
  nodelet's existing `runtime/cri/claims.rs`'s `resource_claim_object_name()`,
  which reads exactly that status field — nodelet already speaks the
  consumer side of this protocol via its own raw-request `RawResourceClaim`
  escape hatch (k8s-openapi v1_33 pin, see `CLAUDE.md`); this controller
  uses the same raw-request approach for the producer side, treating a
  `ResourceClaimTemplate`'s `spec.spec` as an opaque JSON blob it copies
  through rather than modeling the full DRA spec schema. Cleanup of the
  generated `ResourceClaim` needs no dedicated logic here — it carries an
  owner reference back to the Pod, so `garbage-collector-controller`
  (Group D, generic across every discovered kind) already cascades it.
- `device-taint-eviction-controller` (**scoped out**): evicts Pods off
  devices carrying a `DeviceTaintRule` (`resource.k8s.io/v1alpha3`, newer
  than this workspace's `v1_33` k8s-openapi pin and still alpha upstream).
  Neither this project's reference DRA driver
  (`kubernetes-sigs/dra-example-driver`, `deploy/lib/e2e-full-setup.sh`)
  nor any e2e test exercises `DeviceTaintRule` — there is nothing this
  slice could verify against, the same "no infrastructure to prove it
  against" reasoning `docs/GAP_CLOSURE.md` uses elsewhere. Revisit if a
  real device-tainting workflow becomes something this project's e2e
  coverage actually needs.

## I. CSR / cluster PKI — Tier 2 — **implemented (approving, signing, cleaner)**

- `certificatesigningrequest-signing-controller`,
  `-approving-controller`, `-cleaner-controller`
  (`crates/nodecontroller/src/controllers/csr.rs`, **implemented**): the
  approving+signing halves are what nodelet's own TLS bootstrap flow
  (`crates/nodelet/src/bootstrap.rs`, active only when
  `NODELET_BOOTSTRAP_KUBECONFIG` is set) waits on — without them a
  bootstrapping node's CSR sits forever, since nodelet itself "never
  self-approves; approval is entirely the apiserver's job."
- The **one** group with a real external dependency this plan can't
  design away: signing needs the cluster CA *key*, not just its cert.
  Configurable via `NODECONTROLLER_CSR_SIGNING_CA_CERT_PATH`/
  `_KEY_PATH`; left unset, tries a list of well-known candidate paths
  (today: k3s's own `server-ca.{crt,key}`, confirmed as a live
  operational path by the unmerged, profiling-only
  `upstream-kube-apiserver-controller-manager` branch) — a list, not a
  single hardcoded default, since this project won't run on k3s forever.
  Missing/unreadable CA files degrade signing only (approving/cleaning
  keep working) rather than crashing the process.
  Named simplifications, all in the file's own module doc: only the
  `kubernetes.io/kube-apiserver-client-kubelet` signer is handled (the
  only one this project's own stack ever requests); approval is a
  `spec.groups` check for `system:bootstrappers` rather than a real
  `SubjectAccessReview`; no `expirationSeconds` honoring (every cert gets
  `rcgen`'s own default validity); the cleaner uses one flat 1-hour
  terminal-age threshold rather than upstream's per-outcome windows.
- `bootstrap-signer-controller`, `token-cleaner-controller` (**defer**):
  legacy kubeadm bootstrap-token flow, low value now that projected SA
  tokens are the default join mechanism.

## J. Autoscaling & disruption — Tier 2 — **disruption-controller implemented; HPA scoped out**

- `disruption-controller` (`crates/nodecontroller/src/controllers/disruption.rs`,
  **implemented**): keeps `PodDisruptionBudget.status`
  (`currentHealthy`/`desiredHealthy`/`disruptionsAllowed`/`expectedPods`)
  current. This controller never blocks an eviction itself — that's the
  apiserver's own `policy/v1` eviction-subresource admission, reading
  `status.disruptionsAllowed`; without this controller that field never
  updates, so `kubectl drain` and similar tooling see a permanently stale
  value. Named simplifications: healthy means `Ready=True` only, no
  `unhealthyPodEvictionPolicy` distinction (that policy only affects the
  apiserver's own admission decision, out of this controller's scope
  entirely); no `status.conditions`/`disruptedPods` bookkeeping (upstream
  niceties nothing in this project reads); a PDB with neither
  `minAvailable` nor `maxUnavailable` set (invalid per upstream's own
  validation, not re-validated here) fails open rather than closed.
- `horizontalpodautoscaler-controller` (**scoped out**): needs a live
  metrics API (`metrics-server` or equivalent) to be useful at all, and
  unlike Groups G/H's CSI/DRA reference drivers, nothing in this project's
  e2e infrastructure installs one — there is no real infrastructure to
  verify an actual scaling decision against, the same "no infrastructure
  to prove it against" reasoning `device-taint-eviction-controller`
  above uses. Revisit if `deploy/lib/e2e-full-setup.sh` ever grows a
  metrics-server leg.

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
