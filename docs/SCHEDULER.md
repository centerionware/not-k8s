# nodescheduler — design

kube-scheduler's job: watch for pods nobody has placed yet, pick a node for
each, write the `Binding`. This document is the scope and the invariants;
`crates/nodescheduler/src/cycle.rs` is the file to read first in the code, the
way `crates/nodestore/src/command.rs` is for the datastore.

Target is **genuine 1:1 behavioural parity with upstream kube-scheduler at
v1.33**, verified against real workloads by `deploy/lib/test/cases/scheduler.sh`,
not claimed from this document. The parity surface was derived from
`kubernetes/kubernetes` `release-1.33` source rather than the published docs —
the v1.33 reference docs' default-plugin table is stale in two ways that
matter (see "Where the docs are wrong" below).

## Why this component exists

The project's thesis is that a lean event-driven replacement wins on idle cost.
For the scheduler that claim has to be made precisely, because upstream's
scheduler is *already* mostly event-driven — it is not kubelet, and there is no
PLEG-shaped waste to delete. The honest accounting:

Upstream kube-scheduler at idle has exactly **three** unconditional timers,
plus leader election:

| Timer | Period | What we do |
|---|---|---|
| `flushBackoffQCompleted` | 1s | **Eliminated.** A binary heap ordered by expiry plus one `tokio::time::sleep_until` rearmed on push. Same semantics, zero idle wakeups. |
| `cleanupAssumedPods` | 1s | **Eliminated outright.** Assumed pods have no TTL at 1.33 (`durationToExpireAssumedPod = 0`, changed after k/k#106361 caused resource double-booking), so this ticker is already a no-op upstream. We do not have it at all. |
| `flushUnschedulablePodsLeftover` | 30s, evicting at 5m | **Kept, but instrumented.** This is the safety net for incomplete QueueingHints and nothing else. We keep it for parity and for safety, log at `warn` with the pod and the plugin that stranded it whenever it actually rescues something, and count it. In a correct implementation it fires at 0 Hz; if it ever rescues a pod, that is a bug report, not a routine event. |
| leader election renew | 2s | Irreducible. We do HA (see below), so we pay it. |

**So the timers are not where the win is.** The real idle cost of a scheduler
is watch-event decoding, and it is dominated by two things: kubelet node-status
heartbeats (one per node per 10s in this project's control plane —
`setup-control-plane.sh` sets `node-monitor-period=10s`) and pod status churn.

The lever is upstream's `ActionType` bitmask. A Node update that only bumps
`status.conditions[Ready].lastHeartbeatTime` must decompose to **no** action
bits that any plugin subscribed to, and therefore wake nothing and requeue
nothing. Getting that diff exactly right — `UpdateNodeCondition` only when a
condition's `status` actually changed, not its timestamp — is where this
component earns its footprint. Naïvely emitting a generic `Update` for every
Node event reintroduces precisely the cost we are claiming to remove, at a rate
proportional to cluster size.

The second lever is **projection**. Upstream caches whole `v1.Pod` and `v1.Node`
objects. Scheduling needs a strict subset: for a pod, its requests, selectors,
affinities, tolerations, spread constraints, priority, gates, volumes, ports,
labels and claims; for a node, allocatable, labels, taints, conditions, images
and used ports. We store the projection, not the object. This matters *more* as
clusters grow, which is the right direction — this project targets ordinary
multi-control-plane, multi-worker clusters, not only single-node edge boxes.

Neither lever requires giving up anything for scale. In particular the runtime
stays the default multi-threaded `#[tokio::main]` that every other component
uses, and upstream's `parallelism` knob (default 16) is preserved for parallel
Filter/Score across large node counts.

## Invariants

These are load-bearing. They are stated here and enforced at the type level
where possible, in the same spirit as `nodestore`'s determinism rules.

1. **The scheduling cycle is pure over a snapshot.** `PreFilter`, `Filter`,
   `PreScore`, `Score` and `NormalizeScore` read only the `Snapshot` handed to
   them and the pod being scheduled. No I/O, no clock, no RNG, no environment.
   Every plugin at those points is a pure function, which is what makes the
   whole scoring path unit-testable without a cluster — the same reason
   `nodeproxy`'s `build_ruleset()` takes a `State` by value.

2. **Nondeterminism is resolved outside the cycle and passed in.** Two places
   genuinely need randomness — `selectHost`'s reservoir sampling among tied
   nodes, and preemption's random `offset` into the candidate node list. Both
   take their randomness as an explicit parameter chosen by the caller before
   the cycle begins, so a test can pin it. This mirrors `Command::LeaseGrant`
   carrying a leader-chosen id rather than each replica inventing one.

3. **One pod at a time in the scheduling cycle; many in flight in the binding
   cycle.** `Reserve` and `Permit` run *in the scheduling cycle*, synchronously
   — a `Reserve` plugin may assume no other pod is mid-cycle. Only waiting on a
   `Permit` verdict, and everything from `PreBind` onward, is asynchronous.
   Every summary that places Reserve/Permit "in the binding cycle" is wrong for
   implementation purposes.

4. **`PreBind` must never block the scheduling loop.** `VolumeBinding`'s
   `PreBind` polls for PVC binding with a 600s default timeout. That is the
   single genuinely blocking wait in the design, and it lives on the pod's own
   task. Making binding synchronous would let one slow provisioner stall
   placement cluster-wide.

5. **Assumed pods never expire.** Forgetting is driven only by binding failure
   (`ForgetPod`) or by the informer delivering the real bound pod. Do not
   reintroduce a TTL; upstream removed it because it double-booked resources
   when the apiserver was slow.

6. **A pod rejected by plugin P is only ever requeued by events P registered.**
   A missing `EventsToRegister` entry is not an error — it is a silent stall
   until the 5-minute safety net. Every plugin's registered event set is
   therefore covered by a unit test asserting the exact set, so adding a
   rejection reason without adding its wake-up event fails the build rather
   than the cluster.

## Structure

Semantics, ordering and translation are separated the way `nodestore` separates
`store.rs` / `consensus.rs` / `server/`. A behavioural decision made in
`watch.rs` is almost always in the wrong place.

```
crates/nodescheduler/src/
  lib.rs              run(): config, client, leader election, wiring
  config.rs           NODESCHEDULER_* env + KubeSchedulerConfiguration, profiles
  cycle.rs            THE INVARIANTS. The scheduling cycle itself.
  binder.rs           binding cycle: WaitOnPermit → PreBind → Bind → PostBind
  election.rs         Lease-based leader election (new to this repo)
  watch.rs            informers → ClusterEvent. Translation only.
  events.rs           ClusterEvent, EventResource, ActionType bitmask + the diff
  cache/
    node.rs           NodeInfo projection: allocatable, requested, taints,
                      labels, images, used ports, pod refs
    pod.rs            PodInfo projection
    snapshot.rs       generation-based incremental snapshot
    assume.rs         assumed-pod bookkeeping
  queue/
    mod.rs            activeQ / backoffQ / unschedulablePods, in-flight replay
    backoff.rs        expiry heap + single rearmed timer (no 1s tick)
    hints.rs          QueueingHint registry and isPodWorthRequeuing
  framework/
    mod.rs            extension point traits, the runner, plugin registry
    status.rs         Status/Code: Success, Error, Unschedulable,
                      UnschedulableAndUnresolvable, Skip, Wait, Pending
    plugins/          one file per plugin
  preempt.rs          DefaultPreemption
```

## Scope: the default plugin set at v1.33

Taken from `pkg/scheduler/apis/config/v1/default_plugins.go`, expressed as
MultiPoint upstream. Score weights in parentheses.

```
SchedulingGates                          NodeResourcesFit (1)
PrioritySort                             VolumeRestrictions
NodeUnschedulable                        NodeVolumeLimits
NodeName                                 VolumeBinding
TaintToleration (3)                      VolumeZone
NodeAffinity (2)                         PodTopologySpread (2)
NodePorts                                InterPodAffinity (2)
DefaultPreemption                        NodeResourcesBalancedAllocation (1)
ImageLocality (1)                        DefaultBinder
```

Plus `DynamicResources` when DRA is enabled, inserted **immediately before
DefaultPreemption** — deliberately, so that freeing an idle `ResourceClaim`
is tried before evicting a pod that is doing useful work.

### Where the docs are wrong

Two divergences between the v1.33 published reference and the v1.33 source.
The source wins; implement from it.

- The docs still list `EBSLimits`, `GCEPDLimits`, `AzureDiskLimits` as default
  filter plugins. They are **not registered at all** in `release-1.33` — the
  in-tree cloud volume plugins went with in-tree provider removal. Only
  `NodeVolumeLimits` (CSI) survives.
- The docs show a score-weight table where everything is weight 1 except
  PodTopologySpread. The real weights are TaintToleration 3, NodeAffinity 2,
  PodTopologySpread 2, InterPodAffinity 2, the rest 1.

A third trap, not a doc error but a widely-repeated wrong constant: scoring
substitutes **100m CPU / 200Mi memory** for unspecified requests
(`GetNonzeroRequests`), not 1000m/128Mi. It applies to scoring but not
filtering, and mixing the two changes bin-packing subtly.

## Phasing

1:1 parity is not one PR. Each phase is independently mergeable through the
full gate (branch → test → build.yml → e2e → merge) and leaves a cluster that
works, just with less of the surface covered.

**Phase 1 — a scheduler that schedules.** Framework traits, the projection
cache and generation-based snapshot, the event-driven queue with the full
`ActionType` bit decomposition, leader election, `DefaultBinder`, and the
plugins that need no extra informers: `PrioritySort`, `SchedulingGates`,
`NodeUnschedulable`, `NodeName`, `TaintToleration`, `NodeAffinity`,
`NodePorts`, `NodeResourcesFit`, `NodeResourcesBalancedAllocation`,
`ImageLocality`. Two informers total: Pod and Node. This is a genuinely usable
scheduler for a cluster without PVs, spread constraints or preemption, and it
is where the footprint claim gets measured.

**Phase 2 — topology.** ✅ Implemented: `PodTopologySpread` and
`InterPodAffinity`, both with the `AddPod`/`RemovePod` extensions preemption
depends on.

Two deliberate gaps, both narrower than they sound:

  * **System default constraints** are not applied. They need the
    Service/ReplicaSet/ReplicationController/StatefulSet listers, whose only
    consumer is that feature, and because they are `ScheduleAnyway` their
    absence changes **scores and never feasibility** — no pod is placed that
    upstream would refuse, none refused that upstream would place.
    `NODESCHEDULER_TOPOLOGY_DEFAULTING` selects the behaviour.
  * **`namespaceSelector`** on a pod affinity term needs a Namespace watch
    this scheduler does not run. Such terms fail *open* (`selector.rs`'s
    `NeedsNamespaceLister`): over-matching can only refuse a placement, while
    under-matching would silently disable a rule the author wrote and
    co-locate pods meant to be kept apart.

**Phase 3 — preemption.** ✅ Implemented. The PDB watch, `NominatedNodeName`,
nominated-pod injection during Filter, victim selection with reprieve, and the
six-way node choice.

One structural deviation from upstream, with no behavioural difference:
preemption is driven from `cycle.rs` rather than being a `PostFilter` plugin.
Its dry runs must re-run the *Filter* plugins against a hypothetical pod set,
and a plugin is not handed the other plugins — upstream solves that by passing
a framework `Handle` into every plugin, which is a much larger surface than
this crate needs for one caller. Every rule still lives in `preempt.rs`; only
the part that needs the registry lives in the cycle. It still runs exactly
when zero nodes were feasible, still considers only nodes rejected
`Unschedulable`, and still picks the same victims.

**Phase 4 — storage.** `VolumeBinding`, `VolumeZone`, `VolumeRestrictions`,
`NodeVolumeLimits`, and the PV/PVC/StorageClass/CSINode/CSIDriver/
CSIStorageCapacity informers. The reference CSI driver the e2e harness already
installs (`e2e-full-setup.sh`) is what proves this.

**Phase 5 — DRA, profiles, extenders.** `DynamicResources`, multi-profile
`schedulerName` dispatch, and HTTP extenders.

DRA needs the raw-request escape hatch: `resource.k8s.io/v1` does not exist in
the pinned `k8s-openapi` v1_33 schema (only `v1alpha3`/`v1beta1`/`v1beta2`), so
`ResourceClaim` access goes through a hand-written struct and
`client.request()`, exactly as `crates/nodelet/src/runtime/cri/claims.rs`
already does. Do not bump the schema pin; see CLAUDE.md.

The DRA contract with `nodelet` is worth stating explicitly because the two
halves live in different components: the scheduler writes
`status.allocation` + `status.reservedFor` in `PreBind`, before the Binding, and
`nodelet` then calls the driver's `NodePrepareResources` with those results. The
scheduler must never write an allocation for a node it is not about to bind to,
and must roll it back on bind failure — otherwise nodelet sees a claim
allocated to a node that never received the pod, and the device leaks.

## Informers: start only what a plugin asked for

Upstream registers Pod and Node unconditionally and everything else only if
some enabled plugin's `EventsToRegister()` named that resource. We copy that
exactly — it is both the parity behaviour and the footprint behaviour, and it
means `--scheduler=nodescheduler` on a cluster with no PVs costs two watches,
not nine.

One deliberate knob beyond parity: `PodTopologySpread`'s *default constraints*
are the sole consumer of the Service, ReplicationController, ReplicaSet and
StatefulSet informers, and because they are `ScheduleAnyway` they affect
**scoring only, never feasibility**. Parity keeps them (`defaultingType:
SystemDefaulting` is the upstream default, and it does populate those
constraints). `NODESCHEDULER_TOPOLOGY_DEFAULTING=None` turns them off and drops
four informers, at the cost of slightly different scores on pods that declare no
constraints of their own. That is a documented, opt-in divergence — off by
default, because the default has to be parity.

## Correctness details most likely to be got wrong

Recorded here because each one fails silently or load-dependently rather than
loudly, and each already has a test planned against it.

- **`Unschedulable` vs `UnschedulableAndUnresolvable`** gates whether PostFilter
  runs at all and whether a node is a preemption candidate.
- **Nominated-pod injection during Filter.** When filtering node *N* for pod
  *P*, re-run PreFilter `AddPod` for every higher-or-equal-priority pod
  nominated to *N* first. Omit it and two preemptors both claim the same freed
  capacity.
- **In-flight event replay.** Events arriving *during* a pod's cycle must be
  recorded against that pod's marker and replayed when it lands in
  `unschedulablePods`, or it stalls until the 5-minute net.
- **`nextStartNodeIndex` advances by nodes *processed*, not nodes found
  feasible.** Without it, `percentageOfNodesToScore < 100` examines the same
  prefix forever and the tail of the cluster never receives pods. This is
  correctness, not fairness.
- **`selectHost` uses reservoir sampling** among tied nodes. First-wins
  hot-spots badly on a fresh homogeneous cluster.
- **PDB accounting** exempts pods already in `status.disruptedPods` from
  decrementing `DisruptionsAllowed` — the disruption is already booked.
- **`reprievePod` ordering** (PDB-violating victims reprieved before
  non-violating) changes which pods die, with no failure in the obvious tests.
- **Snapshot must be incremental.** Generation-numbered `NodeInfo`s in a
  most-recently-modified-first list; stop at the first generation already in the
  snapshot. A full copy per cycle is the single biggest scalability mistake
  available here — 3 changed nodes must cost 3 copies, not 5000.

## Leader election

New to this repo. Nothing here does leader election today: `nodelet`'s Lease is
node liveness written with server-side apply and `.force()`, which is the
*opposite* of what this needs.

A scheduler is a single writer of `Binding`. With multiple control-plane nodes
— which this project targets — exactly one instance may be scheduling at a
time. `election.rs` implements the standard `coordination.k8s.io/v1` Lease
protocol with optimistic concurrency on `resourceVersion` (not SSA), matching
upstream's defaults: `leaseDuration: 15s`, `renewDeadline: 10s`,
`retryPeriod: 2s`, lock name `kube-scheduler` in `kube-system`.

A non-leader holds no informers open and runs no queue; it acquires, then
builds. Losing the lease means stopping immediately — a scheduler that keeps
binding after losing leadership is the double-binding race the whole mechanism
exists to prevent.

## Interaction with k3s

`--scheduler=nodescheduler` and k3s's `--disable-scheduler` are two halves of
one switch, wired together in `deploy/setup-control-plane.sh`. Two schedulers
watching the same unbound pods and both writing Bindings is a race whose usual
symptom is a pod bound to a node a different scheduler already rejected —
intermittent, and very hard to read backwards from. Hence the flag defaults to
`none`, and `deploy/lib/run.sh` treats "not wanted" as *remove the service*,
not merely "don't install it".
