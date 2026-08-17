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
   `PreBind` waits for the PVC watch to report binding, with a 600s default
   timeout as a failure ceiling. It does not poll the apiserver. That is the
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

```text
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

```text
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

Plus `DynamicResources`, upstream's own `DRAAdminAccess`-independent default
— here it is unconditional, since this project has no separate feature-gate
mechanism to make it optional. Upstream inserts it **immediately before
DefaultPreemption**, deliberately, so that freeing an idle `ResourceClaim` is
tried before evicting a pod doing useful work. That ordering is real here
too: `DynamicResources` is the only `PostFilterPlugin` registered
(`post_filter: vec![Box::new(dra.clone())]`), and `Scheduler::preempt`'s own
fallback is driven separately from `lib.rs`'s `scheduling_loop`, only after
`schedule_one` itself returns `Unschedulable` — so a claim-freeing dry run
always gets tried before eviction ever does, with no explicit ordering code
needed to guarantee it. See `dynamic_resources.rs`'s module header (its
`PostFilter` section) for what the dry run actually does, and the rest of
Phase 5's scope.

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

**Phase 1 — a scheduler that schedules.** ✅ Implemented. Framework traits, the projection
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

Both of this phase's parity gaps are now closed, and it is worth recording
what they were, because each had a plausible argument for leaving it:

  * **System default constraints** are applied. They need the
    Service/ReplicaSet/ReplicationController/StatefulSet watches, whose only
    consumer is that feature, and because they are `ScheduleAnyway` their
    absence changed **scores and never feasibility** — no pod was placed that
    upstream would refuse, none refused that upstream would place. That made
    them cheap to skip and still meant every pod declaring no constraints of
    its own scored differently from upstream. The selector is derived from the
    workloads that *select* the pod, ANDed, per upstream's `DefaultSelector`;
    a pod no workload selects gets no default constraints at all.
  * **`namespaceSelector`** on a pod affinity term is resolved against real
    Namespace labels. It used to fail *open* — over-matching can only refuse a
    placement, while under-matching silently disables a rule the author wrote
    and co-locates pods meant to be kept apart. That is the right ranking of
    two wrong answers rather than the right answer.

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

**Phase 4 — storage.** ✅ Implemented: `VolumeRestrictions` (the five in-tree
legacy volume-identity conflicts, per node, plus `ReadWriteOncePod`
exclusivity, cluster-wide), `NodeVolumeLimits` (CSI per-driver per-node volume
ceilings from `CSINode`), `VolumeZone` (a PV's legacy zone/region *labels*
against a node's), and `VolumeBinding` (unbound-immediate PVCs block a pod
outright; unbound `WaitForFirstConsumer` PVCs are checked against
`StorageClass.allowedTopologies` and, when a driver opts in,
`CSIStorageCapacity`; an already-bound PV's `nodeAffinity` is enforced;
`PreBind` writes `volume.kubernetes.io/selected-node` and waits for the PVC
watch to report `Bound`).
The PV/PVC/StorageClass/CSINode/CSIDriver/CSIStorageCapacity informers all
start unconditionally now, the same as Pod/Node — see "Informers" below for
what that changes about the footprint claim. The reference CSI driver the
e2e harness already installs (`e2e-full-setup.sh`) is what proves this.

`VolumeBinding` also matches a `PersistentVolumeClaim` against an
already-existing, unclaimed `PersistentVolume` (a static PV — matched by
`storageClassName`/access modes/capacity, an explicit `selector`, or an exact
`spec.volumeName` pre-bind), tried before dynamic provisioning is even
considered, matching upstream's own priority order. Two pods that could both
claim the same free static PV is a real scarce-resource race, the same shape
`DynamicResources`' device assume cache exists for — `Reserve` tentatively
marks the PV it picked, `Unreserve`/`PostBind` release the mark. `PreBind`
writes `PersistentVolumeClaim.spec.volumeName` for a static claim (the
built-in PV binder controller completes the actual bind, including
`PersistentVolume.spec.claimRef`, from that alone) instead of the
`selected-node` annotation dynamic provisioning uses. See
`volume_binding.rs`'s module header for the full accounting.

**Phase 5 — DRA, profiles, extenders.**

`DynamicResources` (DRA) is ✅ implemented, real CEL and all: a claim's
`spec.devices.requests[]` — `deviceClassName` + `count`
(`allocationMode: ExactCount`, the default) — is evaluated against real
`ResourceSlice` device inventory using an actual CEL interpreter
(`cel-interpreter`, a new dependency scoped to exactly this — see
`framework/plugins/dynamic_resources.rs`'s module header for why a
pattern-matched subset of CEL wasn't good enough and the environment this
exposes to a selector). A claim already allocated and reused by a second pod
is handled — only `reservedFor` needs updating. `PreEnqueue` holds a pod
whose template-based claim hasn't been generated yet, the same "not yet
rejected, not yet reached scheduling" reasoning `SchedulingGates` uses.
`PreBind` writes `status.allocation` + `status.reservedFor` in one step (DRA
has no external provisioning transition to wait for, unlike VolumeBinding).

`firstAvailable` subrequests, `adminAccess`, `allocationMode: All`,
cross-request `constraints` (`matchAttribute`), and a `ResourceSlice` using
`nodeSelector`/per-device node selection are all ✅ implemented too, each
checked directly against upstream's real allocator
(`k8s.io/dynamic-resource-allocation/structured/allocator.go`) rather than
assumed from the API docs — see `dynamic_resources.rs`'s module header for
the two real bugs that source-reading caught (the
`v1.NodeSelector`-not-`LabelSelector`
type on `ResourceSlice.nodeSelector`, and `ClaimPlan::Nothing` never
re-checking an existing allocation's topology on a node that already held
the reservation). Device selection now uses the same exhaustive backtracking
shape as upstream, including rollback across claims and `firstAvailable`
alternatives.

`PostFilter` is ✅ implemented too: a claim already allocated to a topology
no node satisfies, with nothing else still reserving it, gets deallocated so
the next attempt can pick differently — checked against upstream's real
`DynamicResources.PostFilter`. This needed `PostFilterPlugin` to become
`async` (zero existing implementors at the time, so free to widen).

The CEL interpreter has no opaque/custom value variant for upstream's
`apiservercel.Quantity`. `quantity.rs` therefore carries a private canonical
Quantity representation inside a CEL string and implements the same named
methods (`isGreaterThan`/`isLessThan`/`compareTo`/`add`/`sub`/`sign`/
`isInteger`/`asInteger`/`asApproximateFloat`) with arbitrary-precision
rational arithmetic. Equivalent Kubernetes quantity spellings compare equal
and selection never loses precision; see `dynamic_resources.rs`'s module
header for the representation boundary.

DRA needs the raw-request escape hatch: `resource.k8s.io/v1` does not exist in
the pinned `k8s-openapi` v1_33 schema (only `v1alpha3`/`v1beta1`/`v1beta2`), so
`ResourceClaim`/`DeviceClass`/`ResourceSlice` access goes through hand-written
structs (`cache/dra.rs`) rather than typed `kube-openapi` generated ones —
same pattern `crates/nodelet/src/runtime/cri/claims.rs` uses on the node
side, extended here to also *write*, and to be watchable: each struct
implements `k8s_openapi::Resource`/`Metadata` by hand so `kube::Api`/
`kube::runtime::watcher` work on it exactly like any generated type, via
`kube-core`'s existing blanket impl. Do not bump the schema pin; see
CLAUDE.md.

The DRA contract with `nodelet` is worth stating explicitly because the two
halves live in different components: the scheduler writes
`status.allocation` + `status.reservedFor` in `PreBind`, before the Binding, and
`nodelet` then calls the driver's `NodePrepareResources` with those results. The
scheduler must never write an allocation for a node it is not about to bind to,
and must roll it back on bind failure — otherwise nodelet sees a claim
allocated to a node that never received the pod, and the device leaks.

**Multi-profile `schedulerName` dispatch is ✅ implemented.**
`NODESCHEDULER_PROFILE_NAME` accepts a comma-separated list; every name gets
its own `Registry` (built from the same `default_registry` blueprint — this
crate has no per-profile plugin configuration, so there is nothing to
actually differ between them beyond the name), all sharing one queue, one
`QueueSort`/`PreEnqueue` chain, and one watch layer. `watch.rs`'s
`route_pod` queues a pod naming *any* of the configured profiles;
`lib.rs`'s `scheduling_loop` resolves the right `Registry` per popped pod
from `pod.scheduler_name` before running its cycle. `cycle.rs`'s `Scheduler`
deliberately holds no `Registry` of its own — see its module header's
"Multiple profiles, one `Scheduler`" — because the sweep position and
preemption's nomination promises have to stay consistent across profiles
that place pods onto the same nodes, and every method that needs a plugin
set now takes it as an explicit parameter instead.

**HTTP extenders are ✅ implemented** (`extender.rs`), configured via
`NODESCHEDULER_EXTENDERS_JSON` — a JSON array using upstream
`KubeSchedulerConfiguration` extender field names (`urlPrefix`, `filterVerb`,
`prioritizeVerb`, `weight`, `nodeCacheCapable`, `ignorable`,
`managedResources`), so an operator's existing extender config needs no
translation beyond wrapping it in an env var. `schedule_one` runs configured
extenders' Filter sequentially right after plugin Filter (narrowing the same
way upstream's `findNodesThatPassExtenders` does — a later extender only
sees nodes an earlier one already accepted) and Prioritize right after plugin
Score (rescaled from the extender's own `[0, 10]` range onto the plugins'
`[0, 100]` one — `score * weight * 10`, matching upstream's real combining
formula in `schedule_one.go`'s `prioritizeNodes` — then added into the
already-weighted plugin totals). `managedResources` is honored against both
requests and limits on normal and init containers. An `ignorable` extender
that errors is logged and skipped rather than failing the cycle.

`bindVerb` and `preemptVerb` use the upstream extender/v1 request and response
shapes. TLS configuration supports `enableHTTPS`, insecure verification,
custom CA data/files, client certificate/key data/files, and Go-duration
`httpTimeout`, including `tlsConfig.serverName` as an independent SNI and
certificate-verification name while connections and the HTTP Host header stay
pointed at the configured endpoint.

## Informers: start only what a plugin asked for

Upstream registers Pod and Node unconditionally and everything else only if
some enabled plugin's `EventsToRegister()` named that resource. We copy that
exactly. Through Phase 3 that meant `--scheduler=nodescheduler` on a cluster
with no PVs cost two watches, not nine; Phase 4's four storage plugins are
themselves unconditional default-profile plugins (there is no "no storage
filters" mode, upstream included), so their six informers now start
unconditionally too — the same footprint upstream itself pays once its
default profile is running, PVs or not.

One deliberate knob beyond parity: `PodTopologySpread`'s *default constraints*
are the sole consumer of the Service, ReplicationController, ReplicaSet and
StatefulSet informers, and because they are `ScheduleAnyway` they affect
**scoring only, never feasibility**. Parity keeps them, and is the default
(`defaultingType: SystemDefaulting` is upstream's default too).
`NODESCHEDULER_TOPOLOGY_DEFAULTING=None` turns them off and genuinely drops
those four watches — `watch.rs` starts a `stream::pending()` in their place
rather than starting a watch whose events are ignored — at the cost of slightly
different scores on pods that declare no constraints of their own. That is a
documented, opt-in divergence, off by default, because the default has to be
parity.

The Namespace watch has no such knob: `namespaceSelector` is unconditional, so
it is unconditional too. One watch of small, rarely-changing objects.

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
