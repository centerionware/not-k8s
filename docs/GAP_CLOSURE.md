# kubelet parity gap closure — working memory

Started 2026-07-30, rescoped 2026-07-30 (same day, scope expanded from "close
3 known gaps" to "100% kubelet parity, performance-focused"). Checkpoint
commit before this rescoping: `fdf003b`.

## Verified scope boundary (checked against kubernetes.io docs, not assumed)

Sources: [Kubernetes Components](https://kubernetes.io/docs/concepts/overview/components/),
[kubelet reference](https://kubernetes.io/docs/reference/command-line-tools-reference/kubelet/),
[Node-pressure eviction](https://kubernetes.io/docs/concepts/scheduling-eviction/node-pressure-eviction/),
[Graceful Node Shutdown](https://kubernetes.io/blog/2021/04/21/graceful-node-shutdown-beta/),
[Static Pods](https://kubernetes.io/docs/tasks/configure-pod-container/static-pod/).

**Confirmed genuinely NOT kubelet's job** (someone else's component, not a
nodelet gap):
- Pod scheduling/binding decisions → **kube-scheduler**.
- etcd storage, Raft quorum, peer election → **etcd**.
- ReplicaSet/Deployment/StatefulSet/Job/CronJob/HPA control loops → **kube-controller-manager**.
- Node lifecycle taints after missed heartbeats, cloud taint lifecycle → **kube-controller-manager** (nodelet already correctly does the one thing that *is* its job here: clearing the `node.cloudprovider.kubernetes.io/uninitialized` taint on itself — see `node.rs::clear_cloudprovider_taint`).
- Cloud load balancer / route provisioning → **cloud-controller-manager**.
- Admission control, webhooks, server-side apply field management → **kube-apiserver**.
- CSR approval / cluster CA signing → **kube-controller-manager** + **kube-apiserver**.
- Dynamic PV provisioning → external CSI provisioner sidecar (not kubelet); kubelet's actual job here is only the node-local `NodeStageVolume`/`NodePublishVolume` mount, which *is* in scope below.
- Service/Endpoints → **kube-proxy**'s job in stock Kubernetes; nodelet already reimplements this itself (`svc.rs`, nftables) as a deliberate architectural choice predating this doc — stays as-is, not touched by this pass.
- Windows node support — out of scope for a different reason: this project is Linux-only edge hardware by design (cgroup v2, `/proc`, Linux CRI sockets), not "someone else's job."

Everything else below **is** kubelet's job and is now in scope.

## Round 19: CSI attach coordination (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-18). Offered "CSI
Controller service (attach/detach)", device plugin polish (`GetPreferredAllocation`/
`PreStartContainer`), extending Topology Manager's `restricted` policy to
the multi-node case, or pause; user picked CSI's Controller service — the
one remaining entirely-missing CSI capability.

Re-verified against kubernetes-csi docs before implementing (same
"checked, not assumed" posture as the scope boundary above) and found the
framing in the AskUserQuestion offer was imprecise: **real kubelet never
calls `ControllerPublishVolume`/`ControllerUnpublishVolume` itself** —
those are the **external-attacher** sidecar's job, watching
`VolumeAttachment` objects that **kube-controller-manager**'s
AttachDetachController creates/deletes. This is the same "not kubelet's
job" boundary already established for dynamic provisioning
(external-provisioner) elsewhere in this doc — implementing the Controller
service client itself would have been scope creep into another
component's responsibility, not a nodelet gap.

What **is** kubelet's (and so nodelet's) job on the node side of attach,
and what this round actually implements:
- Check `CSIDriver.spec.attachRequired` for the volume's driver — defaults
  to "attach required" if the object is missing or the field is unset
  (matching upstream's own default).
- If attach isn't required (the common case for node-local/edge storage —
  `csi_pvc.sh`'s existing coverage), behavior is unchanged from round 13.
- If attach **is** required, list `VolumeAttachment` objects and find the
  one matching `(attacher == driver, nodeName == this node,
  source.persistentVolumeName == the bound PV)`. Not yet found, or found
  but `status.attached == false`: `Ok(None)` (same "retry next reconcile"
  treatment every other not-ready-yet PVC/PV state already gets, logged
  with which of the two it is). Found and attached: thread
  `status.attachmentMetadata` through as `publish_context` on both
  `NodeStageVolume` and `NodePublishVolume` (previously hardcoded to
  empty) — some drivers require this (e.g. a device path chosen at attach
  time) for Stage/Publish to succeed at all.
- **`CsiVolumeSource` gained a `publish_context: HashMap<String, String>`
  field**; `resolve_csi_source()` in `runtime/cri.rs` computes it via two
  new pure helpers (`attach_required()`, `find_volume_attachment()`,
  `attachment_publish_context()`) plus a `driver_requires_attach()`
  cluster lookup.
- **`CriRuntime` gained a `node_name` field** (threaded through
  `connect()`) — needed to match `VolumeAttachment.spec.nodeName` against
  this node; nothing in `resolve_csi_source()` previously needed to know
  the node's own name.
- `runtime/csi.rs`'s module doc comment corrected: it previously implied
  the Controller service was "out of scope, not kubelet's job" without
  distinguishing *calling* it (still correctly out of scope) from
  *consuming what it produces* (the actual gap, now closed).

11 new unit tests (`runtime/cri_tests/csi_attach.rs`): `attach_required()`'s
missing-object/unset-field/explicit-true/explicit-false cases,
`find_volume_attachment()`'s match/no-match/empty-list cases,
`attachment_publish_context()`'s no-status/not-attached/attached-empty/
attached-with-metadata cases. 570 tests passing with `--features cri` (up
from 559), 179 mock-only (unchanged — entirely `cri`-gated).
`deploy/lib/test/cases/csi_attach.sh` added — provisions a PVC against an
attach-requiring StorageClass, waits for the pod to reach Running, then
asserts the matching `VolumeAttachment.status.attached` is `true` (proof
the pod only started because nodelet actually found and waited on it, not
a coincidence of timing). Needs `TEST_CSI_ATTACH_STORAGE_CLASS` — a real
attach-requiring driver with a working external-attacher, same class of
infra dependency `csi_pvc.sh` already has for provisioning.

**Confidence note**: `attach_required()`/`find_volume_attachment()`/
`attachment_publish_context()` are pure and unit-tested against
hand-built `CSIDriver`/`VolumeAttachment` objects — solid confidence
there. Not validated live: no real attach-requiring CSI driver or
external-attacher exists in this sandbox (edge/single-node target, no
cloud block storage), so the actual `driver_requires_attach()` cluster
lookup and the end-to-end wait-then-mount flow are unexercised outside
the e2e test's manual-invocation path, same caveat every CSI-adjacent
round since 12 has carried.

## Round 18: Memory Manager (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-17). Offered Memory
Manager, extending Topology Manager's `restricted` policy to the
multi-node case, CSI's Controller service, or device plugin polish; user
picked Memory Manager — the last of the three upstream managers, and it
reuses round 17's NUMA-topology groundwork directly rather than opening
new subsystem work, closing out the CPU/Memory/Topology manager thread
entirely.

- **New `memory_manager.rs`** (`cri`-gated): NUMA node pinning
  (`cpuset_mems`) for Guaranteed-QoS containers with a memory limit set —
  `wants_pinned_memory()` mirrors `cpu_manager.rs::wants_exclusive_cpus()`'s
  eligibility check, minus the "whole number" requirement (memory has no
  integer analog; any positive limit qualifies). `allocate()`/
  `allocate_preferring()` pick a single NUMA node with enough free
  capacity, trying a Topology Manager-preferred node first.
- **Three real simplifications, documented in the module's own doc
  comment rather than silently thin**: (1) **never spans multiple NUMA
  nodes** — real Memory Manager can split a container's memory across
  nodes if no single one has room; this always picks one node or falls
  back to unconstrained, the same graceful-degradation choice CPU Manager
  already makes; (2) **no shared-pool tracking for non-pinned
  containers** — CPU Manager explicitly sets every container's
  `cpuset_cpus` and retroactively updates already-running ones
  (`refresh_shared_pool_cpusets()`, round 16); Memory Manager only ever
  touches `cpuset_mems` on containers it actually pins, leaving everything
  else unconstrained (memory-safe, just less strictly isolated); (3) **no
  per-NUMA-node `--reserved-memory`-equivalent** — only total node
  capacity is tracked, not a separate system/kube reservation carved out
  of a specific node (system/kube-reserved memory is still subtracted
  from `Node.status.allocatable` overall, just not pinned away from any
  particular NUMA node).
- **`topology.rs` gained `read_numa_memory()`/`memory_hint()`** — reads
  each NUMA node's `MemTotal` from `/sys/devices/system/node/node*/meminfo`
  (validated with a real unit test against this sandbox's own host, same
  confidence level round 17's CPU topology read already had), and Memory
  Manager is now a third hint provider alongside CPU Manager and device
  plugins in `create_and_start_container()`'s alignment computation. The
  module's "No Memory Manager exists" caveat from round 17 is gone.
- **Wired into `runtime/cri.rs::create_and_start_container()`** right
  alongside the existing CPU Manager block — computes `wants_pinned_memory`
  early (used both for the Topology Manager hint and the actual
  allocation), sets `resources.cpuset_mems` on success, releases on
  `CreateContainer` failure (mirrors the CPU Manager rollback) and via the
  same `release_container_devices()`/`release_sandbox_devices()` helpers
  every other per-container resource already uses.
- 26 new unit tests: `memory_manager.rs`'s `wants_pinned_memory` and a
  full allocate/release/disjointness/preference suite mirroring
  `cpu_manager_tests`'s structure, plus `topology.rs`'s
  `read_numa_memory`/`memory_hint`.

559 tests passing with `--features cri` (up from 533), 179 mock-only
(unchanged — entirely `cri`-gated).
`deploy/lib/test/cases/memory_manager.sh` added — creates a Guaranteed
pod with a memory limit and checks its `cpuset.mems` cgroup file (found
by container ID, same technique `cpu_manager.sh` already uses) is
non-empty. Needs `TEST_MEMORY_MANAGER_STATIC=true`.

**Confidence note, same posture as round 17**: `read_numa_memory()` is
validated against this sandbox's real `/sys/devices/system/node`
directly. What's not validated live: genuine multi-container NUMA
capacity contention (this sandbox's single NUMA node means every
allocation trivially succeeds) and the "no single node has enough room,
falls back to unconstrained" path actually triggering for real. All
failure modes are logged warnings with the container left unpinned,
never a crash.

## Round 17: Topology Manager (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-16). Offered Topology
Manager, Memory Manager, CSI's Controller service, or device plugin
polish; user picked Topology Manager — it ties together CPU Manager
(rounds 15-16) and device plugins (round 14) rather than opening a new
subsystem, and Memory Manager would need the same NUMA-topology
groundwork this round already builds, so doing Topology Manager first
avoids duplicating that work.

- **New `topology.rs`** (`cri`-gated): reads real NUMA topology from
  `/sys/devices/system/node/node*/cpulist` (`read_numa_topology()`,
  parsed once at startup — NUMA layout doesn't change at runtime on any
  hardware nodelet targets), and implements a **single-node-only**
  hint/alignment algorithm — deliberately simpler than upstream's full
  per-provider-bitmask/cross-combination search, documented up front in
  the module doc comment. `cpu_hint()`/`device_hint()` compute which
  individual NUMA nodes can alone satisfy a CPU or device request;
  `align()` intersects every hint provider's candidate set and returns the
  lowest-numbered common node, or `None` if none exists. Reaches the same
  answer as upstream whenever a single node can satisfy everything (the
  common case, and the only case `single-numa-node` policy upstream ever
  accepts either) — the gap is upstream's `restricted` policy also
  accepting valid *multi*-node alignments, which this doesn't search for;
  `restricted` is treated identically to `single-numa-node` here.
- **`DeviceInfo` gained `numa_node: Option<u32>`** — parsed from the
  device plugin's `TopologyInfo.nodes[0]` during `ListAndWatch` (round 14
  only tracked `id`/`healthy`, dropping topology entirely). `None` (no
  `TopologyInfo` reported) is treated as "compatible with every NUMA
  node," not "compatible with none," matching upstream's own device
  manager hint generation.
- **`CpuManager::allocate_preferring()`/`DevicePlugins::allocate_preferring()`**
  — both gained a NUMA-preference parameter: try the aligned node's own
  CPUs/devices first, falling back to the rest of the pool if the
  preferred node alone can't supply the full count. `allocate()` on both
  is now a thin wrapper calling these with `None` (Topology Manager
  disabled — identical behavior to before this round).
- **Wired into `runtime/cri.rs::create_and_start_container()`**: before
  the existing CPU/device allocation logic runs, computes every hint
  provider's candidate set for *this* container (its exclusive-CPU want,
  if any; each device-plugin resource it requests), intersects them via
  `align()`, and applies the configured policy — `None` skips the whole
  computation (a true no-op, exactly pre-round-17 behavior); `BestEffort`
  logs and proceeds unaligned on failure; `Restricted`/`SingleNumaNode`
  return an error that propagates up through `ensure_container()`/
  `ensure_pod()`, leaving the pod Pending rather than starting it
  misaligned — nodelet's version of upstream's Topology Manager admission
  rejection.
- 25 new unit tests for `topology.rs` (`parse_cpulist`, `read_numa_topology`
  — including a real read against `/sys/devices/system/node` on this
  host, `cpu_hint`/`device_hint`/`align`), plus 4 new `CpuManager` tests
  and 4 new `pick_devices_preferring` tests for the NUMA-preference paths.

533 tests passing with `--features cri` (up from 500), 179 mock-only
(unchanged — entirely `cri`-gated).
`deploy/lib/test/cases/topology_manager.sh` added: an automated check
that `single-numa-node` policy never spuriously rejects a pod on a
single-NUMA-node host (this sandbox's own host has exactly one NUMA node
with cpus 0-3, confirmed via the real-topology unit test above), plus a
manual-note for genuine cross-provider alignment verification (needs
real multi-socket hardware or a NUMA-aware device plugin, neither
available here).

**Confidence note**: the NUMA-topology *reading* (`read_numa_topology()`)
is validated against this sandbox's real `/sys/devices/system/node`
directly in a unit test — genuine confidence there, unlike the usual
"no live X in this sandbox" caveat other rounds have carried. What's
*not* validated live is genuine multi-NUMA cross-provider alignment
(CPU + device on the same node) and the `Restricted`/`SingleNumaNode`
rejection path actually firing for a real unsatisfiable request — this
sandbox has only one NUMA node, so every request trivially aligns and
rejection never triggers here. Same failure-safe posture as always:
`None` policy (the default) never touches any of this new code.

## Round 16: CPU Manager retroactive shared-pool updates (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-15). Offered closing
round 15's biggest flagged gap, Topology Manager, Memory Manager, or
smaller polish items (CSI Controller service, device plugin
`GetPreferredAllocation`/`PreStartContainer`); user picked closing the
round-15 gap — it's what makes CPU Manager's isolation guarantee actually
hold in the common case (pods arriving in mixed order) rather than only
for containers created after an exclusive claim.

- **`runtime/cri.rs::refresh_shared_pool_cpusets()`** — after every
  exclusive CPU claim or release, sweeps every currently-tracked,
  non-exclusively-pinned container and calls CRI's
  `UpdateContainerResources` to bring its `cpuset_cpus` in line with the
  now-current shared pool. Diffs against last-known state first (skips a
  container whose cpuset already matches), and skips anything
  `cpu_manager.is_exclusive()` reports as pinned — those keep their own
  dedicated cores untouched.
- **New `container_resources` side table** (`"sandbox_id/container_name"
  -> (container_id, last-applied LinuxContainerResources)`) — needed
  because CRI's `ListContainers` doesn't expose a container's
  currently-applied resources in any structured, cross-runtime way, so
  `UpdateContainerResources` calls would otherwise risk clobbering a
  container's CPU shares/quota/memory limit while only meaning to change
  its cpuset. Recorded once a container's `StartContainer` succeeds,
  removed when it's torn down.
- **`CpuManager::is_exclusive()`** — new query the refresh sweep uses to
  tell "this container is intentionally pinned, leave it alone" apart from
  "this container should track the shared pool."
- **`release_container_devices()`/`release_sandbox_devices()` (round 14)
  are now `async`** — they already released CPU Manager claims (round 15);
  they now also trigger `refresh_shared_pool_cpusets()` afterward so a
  released claim's cores actually become available to whatever's already
  running, not just theoretically free for the next container created.
- 3 new unit tests for `CpuManager::is_exclusive()`. The
  `UpdateContainerResources` call itself isn't independently unit-tested
  (same treatment every other real CRI RPC call in this file gets — only
  the pure decision logic is unit-testable without a live socket); the new
  e2e test below is what actually proves this works end to end.

500 tests passing with `--features cri` (up from 497), 179 mock-only
(unchanged — entirely `cri`-gated).
`deploy/lib/test/cases/cpu_manager.sh` gained a second test:
`test_cpu_manager_retroactively_shrinks_an_already_running_shared_pool_container`
creates a BestEffort pod first, records its `cpuset.cpus`, then creates a
Guaranteed 1-CPU pod and asserts the BestEffort pod's cpuset actually
*changed* afterward — the one thing round 15's test couldn't prove (its
two pods were both created after the policy was already settled, so it
only showed disjoint exclusive assignment, not retroactivity).

**Still not implemented, unchanged from round 15**: topology/socket-aware
CPU selection (still simple ascending-ID) and Topology Manager itself.

## Round 15: CPU Manager (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-14). Offered CPU/Memory/
Topology managers, CSI's Controller service, and device plugins' remaining
gaps; user picked the managers category as "the last major unaddressed
category," and within it, CPU Manager was chosen as the most tractable and
valuable specific piece — CPU pinning matters for edge/latency-sensitive
workloads, Memory Manager is the least commonly used of the three upstream,
and Topology Manager only has something to coordinate once CPU Manager
(and ideally Memory Manager) exist to coordinate between.

- **New `cpu_manager.rs`** (`cri`-gated): implements real kubelet's
  `--cpu-manager-policy=static` — a Guaranteed-QoS container requesting a
  whole number of CPUs (`cpu` request == limit == an integer, the same
  rule upstream uses) gets exclusive cores via CRI's `cpuset_cpus`, picked
  ascending from the current shared pool (total cores minus reserved
  minus already-exclusively-claimed). Opt-in: `NODELET_CPU_MANAGER_POLICY`
  defaults to `none` (upstream's own default) — `cpu_manager` is `None` on
  `CriRuntime` in that case, and every container's `cpuset_cpus` is left
  unset exactly as before this round, a true no-op.
- **Reserved CPUs derived from existing config** — `reserved_cpu_count()`
  reuses `system_reserved_cpu_millicores + kube_reserved_cpu_millicores`
  (round 11), rounded up to a whole core, rather than adding a third
  reservation config just for this. No `--reserved-cpus`-equivalent flag.
- **Every container now gets an explicit `cpuset_cpus`, not just exclusive
  ones** — when the policy is enabled, a container that *doesn't* qualify
  for exclusive cores gets the current shared pool set explicitly (instead
  of an empty/unconstrained cpuset), so newly-created shared-pool
  containers correctly exclude whatever's already exclusively claimed.
- **Deliberately scoped as a first slice, documented in the module's own
  doc comment**: real kubelet's static policy is *bidirectional* — when a
  new exclusive claim is made, every already-running shared-pool
  container gets retroactively shrunk via `UpdateContainerResources`, and
  grown back on release. This round only sets `cpuset_cpus` at
  container-*creation* time; a shared-pool container that was already
  running before a later exclusive claim keeps its original (wider)
  cpuset. Still delivers the core guarantee (an exclusive container's
  cores are never *newly* handed to something else after the fact), just
  not full retroactive isolation from whatever was already running.
  Retroactive `UpdateContainerResources` sweeping, topology/socket-aware
  core selection, and Topology/Memory Manager are explicitly left for a
  future round.
- **New `device_allocations`-shaped side table folded into the existing
  release helpers** — `release_container_devices()`/`release_sandbox_devices()`
  (round 14) now also release any CPU Manager exclusive claim for the
  same key, so one release call at each restart/retry/GC/removal site
  handles both device-plugin and CPU-pinning cleanup instead of needing a
  second parallel set of call sites.
- 26 new unit tests: `wants_exclusive_cpus` (the QoS/whole-CPU eligibility
  rule), `format_cpuset`/`reserved_cpu_count` (cgroup-string rendering and
  reservation rounding), and a full `CpuManager` allocation/release/
  disjointness/reserved-exclusion suite.

497 tests passing with `--features cri` (up from 477), 179 mock-only
(unchanged — this round is entirely `cri`-gated).
`deploy/lib/test/cases/cpu_manager.sh` added — unlike device plugins, this
one *is* fully automatable without special hardware: it creates two
Guaranteed 1-CPU pods and asserts their `cpuset.cpus` cgroup files (found
by container ID under the cgroup tree, tolerant of driver naming) are
non-empty and disjoint. Needs `TEST_CPU_MANAGER_STATIC=true` telling the
suite the running nodelet actually has the policy enabled (same opt-in-
config pattern `static_pods.sh`/`log_rotation.sh` already use).

## Round 14: device plugins (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-13). Offered device
plugins, the CPU/Memory/Topology managers, and CSI's Controller service
(attach/detach); user picked device plugins, reasoning that it reuses
round 13's plugin-registration infrastructure directly (the registration
client already had to *reject* `DevicePlugin` registrations before this
round — now it accepts and routes them) rather than opening entirely new
surface, and has real value for edge devices with attached accelerators.

- **New `device_plugins.rs`** (`cri`-gated): the kubelet-side client for
  the Device Plugin API (`k8s.io/kubelet/pkg/apis/deviceplugin/v1beta1`),
  vendoring a cleaned `proto/deviceplugin.proto` (gogoproto import/options
  and per-field `[(gogoproto.customname)]` annotations stripped — third
  proto this project has needed that treatment for, after `cri.proto` and
  `pluginregistration.proto`). Three responsibilities: **inventory**
  (holds each registered plugin's `ListAndWatch` stream open for the life
  of its registration, tracking device health as it changes — a
  reconnect loop, same 5s-retry shape as other background loops in this
  codebase, handles a plugin process restarting); **capacity
  advertisement** (`capacity_map()` — healthy device counts); **allocation**
  (`allocate()` — picks specific healthy, not-already-allocated device
  IDs and calls the plugin's `Allocate` RPC for them).
- **`plugin_registry.rs` now routes by `PluginInfo.type`** instead of only
  accepting `"CSIPlugin"` — `"DevicePlugin"` registrations go to the new
  `DevicePlugins` registry via the identical `GetInfo`/
  `NotifyRegistrationStatus` handshake CSI drivers already used. Anything
  else still gets a real rejection response, not silence.
- **`Node.status.capacity`/`.allocatable` gained real plumbing** for
  extended resources — `node.rs::build_status()` now takes an
  `extra_capacity: &BTreeMap<String, u64>` parameter, threaded from a new
  `PodRuntime::device_plugin_capacity()` trait method (default: empty, so
  the mock runtime and any future runtime implementation are unaffected).
  This meant widening `node::register()`/`push_status()`'s signatures and
  `main.rs::heartbeat_loop()` to carry the runtime through — a real
  (small) refactor, not just additive.
- **`Allocate()` wired into `runtime/cri.rs::create_and_start_container()`**
  — a new pure `extended_resource_requests()` extracts every non-cpu/
  memory key from a container's `resources.limits`; any of those a
  registered device plugin actually backs gets allocated, and the
  response's envs/mounts/device-nodes/annotations are merged into the
  `ContainerConfig` right alongside everything else already built there
  (CRI's own `ContainerConfig.devices` field turned out to match the
  device plugin API's `DeviceSpec` message almost field-for-field —
  no translation gap to bridge).
- **New side table `device_allocations`** (`"sandbox_id/container_name" ->
  [(resource_name, device_ids)]`, same key shape `restart_counts` already
  uses) — so a container's devices get released back to the pool on
  restart-on-exit, init-container retry, sandbox GC, or pod removal,
  instead of being permanently stranded as "in use." Devices are also
  released immediately if `CreateContainer`/`StartContainer` fails after
  allocation succeeded — a failed container must not strand hardware.
- 30 new unit tests: `device_plugins.rs`'s `pick_devices` (the pure
  selection logic), capacity/registration-state transitions (register/
  deregister/re-register, stale-endpoint rejection); `node.rs`'s
  `build_status()` extra-capacity merge; `runtime/cri.rs`'s
  `extended_resource_requests()`.

477 tests passing with `--features cri` (up from 447), 179 mock-only (up
from 174 — `node.rs::build_status()`'s new tests are mock-buildable since
`node.rs` itself isn't `cri`-gated).
`deploy/lib/test/cases/device_plugins.sh` added: confirms the shared
registration directory exists (proves the watcher started), plus a
manual-note for the full flow — this suite has no GPU/FPGA hardware, and
a real device plugin binary isn't something to bundle in a test harness.
Noted as a natural future improvement: a small hand-rolled fake gRPC
device plugin (`GetInfo`/`ListAndWatch`/`Allocate` are all easy to fake
without real hardware) would make this fully automatable, unlike CSI's
`csi_pvc.sh`, which genuinely needs a real storage backend.

**Same confidence caveat as rounds 12/13**: no real device plugin was
reachable in the environment that built this — verified against the
vendored proto and real kubelet's documented Device Plugin API behavior,
not a live handshake. All failure modes (allocation failure, stream
disconnect, malformed registration) are logged warnings with the
container starting without that device (or, if a plugin's `Allocate`
genuinely can't be satisfied, that one extended-resource request being
silently dropped) — never a crash or a stuck reconcile loop.

## Round 13: dynamic CSI driver discovery + per-volume secrets (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-12). Offered "deepen
PVC/CSI" (round 12's two flagged gaps) against the CPU/Memory/Topology
managers and device plugins; user picked deepening what already exists
over opening new surface.

- **New `plugin_registry.rs`** (`cri`-feature-gated): the client-side half
  of the CSI/DevicePlugin plugin-registration protocol, vendoring a
  cleaned-up `proto/pluginregistration.proto` (from
  `k8s.io/kubelet/pkg/apis/pluginregistration/v1/api.proto` — stripped the
  upstream file's `gogoproto` import/options, which are Go-codegen-only and
  have no prost equivalent, same treatment `cri.proto` already got for a
  protoc-version-specific field option). **The protocol is inverted from
  what the name suggests**: the *plugin* (a CSI driver's
  `node-driver-registrar` sidecar) runs the gRPC server, on a socket it
  creates in a shared watched directory; nodelet is the *client* — it polls
  the directory for new sockets (poll-based, matching `static_pods.rs`/log
  rotation's style rather than pulling in a filesystem-notification
  dependency), dials each one, calls `GetInfo()` to learn the driver's
  name/type/endpoint, and `NotifyRegistrationStatus()` to confirm or
  reject it.
- **`CsiDrivers`'s endpoint map is now mutable** (`Mutex<BTreeMap<...>>`,
  was a plain immutable map seeded once at construction) — new
  `register()`/`deregister()` methods the watcher calls as sockets appear/
  disappear. `NODELET_CSI_DRIVERS` still works exactly as before, as a
  seed for the same map a dynamic registration can now also populate or
  override.
- **Explicit rejection of non-CSI registrations** — device plugins use
  this *exact same* protocol, but nodelet doesn't implement the
  DevicePlugin gRPC API itself. Rather than silently ignore a device
  plugin's registration attempt (which would leave its registrar hanging/
  retrying forever without ever knowing why), `plugin_registry.rs` replies
  with a real `NotifyRegistrationStatus{plugin_registered: false, error:
  "..."}` — same courtesy real kubelet gives a plugin type it doesn't
  support.
- **`nodeStageSecretRef`/`nodePublishSecretRef`** — `runtime/cri.rs`'s
  `resolve_csi_source()` now resolves both (a new
  `resolve_csi_secret_ref()` helper) and threads them through to the CSI
  requests' `secrets` map, closing round 12's other explicitly-flagged
  simplification. `SecretReference.namespace` (optional, since
  `PersistentVolume` is cluster-scoped and has no natural pod namespace to
  inherit) falls back to the PVC's own namespace when unset.
- 6 new unit tests for `plugin_registry.rs`'s `scan_registry_dir()` (real
  `UnixListener` sockets in a scratch directory — genuinely exercises "is
  this actually a socket file," not a mock) and 6 for `CsiDrivers`'
  register/deregister/re-register state transitions.

447 tests passing with `--features cri` (up from 435), 174 mock-only
(unchanged — both additions are `cri`-gated).
`deploy/lib/test/cases/csi_plugin_registration.sh` added: an automated
check that the registry directory actually gets created (proves the
watcher started without erroring) plus a manual-note for the full
registration handshake, which needs a real CSI driver's registrar
pointed at nodelet instead of kubelet — not something this suite can
deploy on the cluster's behalf. `csi_pvc.sh` (round 12) now also
implicitly exercises this: run it *without* `NODELET_CSI_DRIVERS` set,
against a driver whose registrar is pointed at
`NODELET_PLUGIN_REGISTRY_PATH`, and it proves dynamic discovery end to
end if it still passes.

**Same confidence caveat as round 12, unchanged**: no CSI driver (or its
registrar sidecar) was reachable in the environment that built this — the
registration protocol implementation was verified against the vendored
proto and real kubelet's documented behavior, not a live handshake. Same
failure mode as always: a warning, not a crash, and static
`NODELET_CSI_DRIVERS` config keeps working regardless of whether dynamic
discovery ever succeeds.

## Round 12: PersistentVolumeClaim / CSI, first slice (2026-07-31, same day)

Explicitly asked again (same reasoning as round 11 — everything left is
big/invasive): user picked PVC/CSI over the CPU/Memory/Topology managers
and device plugins, calling it "the single biggest remaining feature gap
... real user-facing value," while accepting it as a first-slice/multi-round
effort rather than expecting full CSI support in one pass.

- **New `runtime/csi.rs`**: a minimal CSI Node-service client
  (`NodeStageVolume`/`NodeUnstageVolume`/`NodePublishVolume`/
  `NodeUnpublishVolume`/`NodeGetCapabilities`) using a freshly vendored
  `proto/csi.proto` (the stable v1 API from the upstream
  container-storage-interface/spec repo, tag `v1.9.0`) — compiled the same
  way `proto/cri.proto` already is (`tonic_prost_build`, `cri`-feature-only).
- **Deliberate scope-narrowing, documented up front in the module's own doc
  comment**: real kubelet discovers CSI drivers dynamically — a driver's
  DaemonSet registers its socket against kubelet's own plugin-registration
  gRPC service. Implementing that second server is real additional CSI
  plumbing on top of the Node service itself. This round takes the simpler
  route instead: `NODELET_CSI_DRIVERS=driver-name=unix:///path/to/socket`
  statically maps a driver name to its already-known socket path. This
  still talks to an unmodified, off-the-shelf CSI driver container (the
  registration dance is how kubelet *discovers* the socket, not something
  the Node RPCs themselves depend on) — it just needs that path configured
  up front instead of found automatically.
- **Wired into `runtime/cri.rs`**: `resolve_volumes()` gained a
  `persistent_volume_claim` branch — resolves the PVC (must be
  already-Bound; not-yet-bound is logged and retried next reconcile, same
  as any transient volume-resolution failure) to its `PersistentVolume`,
  extracts `.spec.csi`, calls `CsiDrivers::mount()`, and — on success —
  inserts the publish target path into the same `volume name -> host
  directory` map every other volume kind already populates. No special
  casing needed in `build_mounts()`: NodePublishVolume makes that directory
  a real, populated mountpoint, which then gets bind-mounted into the
  container exactly like a ConfigMap/Secret/emptyDir directory already is.
  `remove_pod()` gained a matching `unmount_csi_volumes()` that
  unpublishes (and, if this was the last pod referencing it, unstages)
  every CSI volume the pod had — best-effort, one failing driver call
  doesn't block the rest of teardown.
- **Per-node reference counting** (`CsiDrivers`'s `refs` map, keyed by
  `(driver, volume_handle)` -> the *set* of pod UIDs using it, not a plain
  counter) — `ensure_pod()`/`resolve_volumes()` runs on every reconcile of
  an already-running pod, not just once at creation, so a plain increment-
  on-mount counter would inflate without bound; a set makes repeated calls
  for the same pod a no-op, and `NodeUnstageVolume` only fires once the set
  is actually empty.
- 12 new unit tests, all pure logic: `has_stage_unstage_capability`
  (decides whether to call NodeStageVolume at all),
  `staging_path`/`mount_capability` (path/request construction). The
  gRPC plumbing itself (`connect_uds`, the actual Stage/Publish/Unpublish/
  Unstage calls) mirrors `runtime/cri.rs`'s existing CRI client exactly and
  is validated the same way that always has been: against a live socket,
  not a unit test — see the confidence note below.

435 tests passing with `--features cri` (up from 423), 174 mock-only
(unchanged — this round's new code is entirely `cri`-gated).
`deploy/lib/test/cases/csi_pvc.sh` added — creates a PVC against
`TEST_CSI_STORAGE_CLASS`, a pod mounting it, and verifies a file the
container wrote lands in the host-materialized volume path. Skips cleanly
without `TEST_CSI_STORAGE_CLASS` set to a StorageClass backed by both a
working external-provisioner *and* a driver also listed in the running
nodelet's `NODELET_CSI_DRIVERS` — real infrastructure this suite can't
stand up itself, same category as the graceful-shutdown and cgroup-write
manual/live-only checks from rounds 9 and 11.

**Known limitation, honestly flagged, same treatment as rounds 9 and 11**:
no CSI driver socket was reachable in the environment that built this, so
none of the actual gRPC calls (`NodeGetCapabilities`, `NodeStageVolume`,
`NodePublishVolume`, `NodeUnpublishVolume`, `NodeUnstageVolume`) have been
exercised against a real driver — only the proto compilation itself and
the pure request-construction logic were verified directly. The CSI wire
protocol is well-specified and this client mirrors the exact shape
`runtime/cri.rs`'s already-working CRI client uses (same `connect_uds`
pattern, same tonic-generated client style), so the main risk isn't
protocol-level, it's driver-specific behavior this slice doesn't handle:
drivers that require `nodeStageSecretRef`/`nodePublishSecretRef`
credentials, drivers whose `NodeGetCapabilities` needs a Controller-service
round-trip first (rare, but not impossible), and any driver-specific
`volume_context`/`publish_context` expectations beyond what's in
`PersistentVolume.spec.csi.volumeAttributes`. All of these fail as a
logged warning + the volume silently absent from the pod's mounts (the
pod still starts, just without that volume) — never a crash or a stuck
reconcile loop.

## Round 11: cgroup/QoS hierarchy + node allocatable enforcement (2026-07-31, same day)

Unlike rounds 6–10 (picked autonomously, "you pick the path"/"continue"),
this one was explicitly asked about first — the remaining list (PVC/CSI,
this round's item, CPU/Memory/Topology managers, device plugins) is bigger
and more invasive than recent rounds, so the user was asked to choose
rather than assuming. They picked this: "a real correctness gap (pods can
currently exceed what real kubelet would allow)" over PVC/CSI (bigger,
multi-round scope) and the CPU/Memory/Topology managers + device plugins
(lower value for nodelet's edge-device target).

- **QoS-scoped `cgroup_parent`** (`cgroup.rs::cgroup_parent_for`) — every
  pod sandbox now gets a real cgroup parent path scoped by QoS class
  (`/kubepods/pod<uid>` Guaranteed, `/kubepods/burstable|besteffort/pod<uid>`
  otherwise), wired into `runtime/cri.rs::ensure_pod` right alongside the
  existing `runtime_handler` resolution. Before this, `LinuxPodSandboxConfig.linux`
  was only ever populated for host-network pods — every other pod's
  sandbox had no `cgroup_parent` at all, so pods landed wherever the
  container runtime's own default happened to put them, with zero
  relationship to QoS class.
- **Key discovery that simplified this a lot**: CRI's own proto comment on
  `cgroup_parent` says "the cgroupfs style syntax will be used, but the
  container runtime can convert it to systemd semantics if needed" — so
  nodelet doesn't need to detect or configure a cgroup driver at all (no
  `NODELET_CGROUP_DRIVER`, unlike real kubelet's `--cgroup-driver` flag).
  It always builds the cgroupfs-style path and trusts the runtime to
  convert it if it's using systemd unit naming internally.
- **Node allocatable enforcement** (`cgroup.rs::enforce_node_allocatable`,
  called once at startup from `main.rs`) — creates and caps the top-level
  `kubepods` cgroup (`cpu.max`/`memory.max`, cgroup v2 only) at the node's
  allocatable resources, so pods collectively can never exceed it
  regardless of what any individual pod's own limits say. This is the
  actual "enforcement" real kubelet's `--enforce-node-allocatable=pods`
  (its own default) gives.
- **`Node.status.allocatable` is now a real computation, not `== capacity`**
  (`node.rs::allocatable_map`) — `capacity - (system-reserved +
  kube-reserved)`, new config: `NODELET_SYSTEM_RESERVED_CPU_MILLICORES`/
  `_MEMORY_BYTES`, `NODELET_KUBE_RESERVED_CPU_MILLICORES`/`_MEMORY_BYTES`
  (all default `0`, matching upstream — reservation is opt-in there too).
  This is a correctness fix independent of the cgroup enforcement above:
  even without cgroup v2 or root, the *reported* allocatable now reflects
  reservations, same as real kubelet's status always has.
- **Bonus, same code path**: `RuntimeClass.overhead` (`spec.overhead`)
  now also gets wired through, via a new `resource_list_to_linux_resources()`
  converting the flat `ResourceList` into `LinuxContainerResources` for
  `LinuxPodSandboxConfig.overhead` — the field existed right next to
  `cgroup_parent` in the same proto message, and the conversion logic was
  a near-identical variant of the existing `linux_resources()`, so this
  closed the previously-🟡 "RuntimeClass Overhead not implemented" note
  from round 7 for free rather than leaving it for its own round.
- 23 new unit tests: `cgroup_parent_for`, `cpu_max_line`/`memory_max_line`,
  `enforce_node_allocatable` (pointed at a scratch directory — proves the
  file layout/content, not real kernel cgroup semantics, see the caveat
  below), `allocatable_map`, `resource_list_to_linux_resources`.

423 tests passing with `--features cri` (up from 397), 174 mock-only (up
from 168 — `allocatable_map` lives in `node.rs`, not `cri`-gated; the rest
of this round's new code is in `cgroup.rs`, which is).
`deploy/lib/test/cases/cgroup_hierarchy.sh` added for live-cluster
validation — checks the `kubepods` cgroup exists with readable
`cpu.max`/`memory.max`, and that a BestEffort pod's cgroup lands somewhere
findable by UID under `kubepods` (tolerant of either cgroupfs or systemd
driver naming, since it can't assume which one a given cluster uses).

**Known limitation, honestly flagged, same treatment as round 9's D-Bus
glue**: `enforce_node_allocatable`'s actual cgroup v2 writes were never
exercised against a real `/sys/fs/cgroup` — this sandbox's cgroup v2 mount
is read-only to a non-root user, so only the pure logic (path building,
`cpu.max`/`memory.max` content formatting, and the file-creation flow
against a scratch directory standing in for the real path) could be
verified directly. The three things most likely to need a look on first
real use: (1) whether `cgroup.subtree_control` on `cgroup_fs_root`'s
top level already has `cpu`/`memory` delegated (a fresh systemd host
usually does; a container without the host's cgroup mount bind-mounted in
may not), (2) whether nodelet's own process has permission to write there
at all (needs root, or an equivalent capability/cgroup namespace grant),
(3) whether a systemd-driver containerd's own management of `kubepods.slice`
conflicts with nodelet also writing directly to a cgroupfs path in the
same tree (untested interaction — real kubelet with `--cgroup-driver=systemd`
uses a `dbus`/`systemd-run` call to set the slice's properties instead of
raw file writes for exactly this reason, which this round's simpler
cgroupfs-direct-write approach doesn't replicate). All three fail safe: a
logged warning, not a crash — `Node.status.allocatable` is still reported
correctly regardless (that computation doesn't touch the filesystem at
all), and pod scheduling/creation is entirely unaffected either way.

## Round 10: `/metrics/resource` + `/metrics/cadvisor` (2026-07-31, same day)

Continued closing gaps ("continue" — no further scoping given). Picked
these next: they were the last two items on `unimplemented.sh`'s active-
placeholder list, both reuse `/stats/summary`'s existing `PodUsage`/
`UsageStats` data (round 7), and neither touches the container-creation
path at all — a low-risk, self-contained follow-up after round 9's larger
D-Bus addition.

- **New `server::prom_metrics`** (`cri`-feature-gated, same as every other
  `server::*` module) — renders Prometheus text-exposition-format output
  from the same `PodUsage` data `/stats/summary` already collects via CRI's
  `ListPodSandboxStats`. No separate collection path for either endpoint.
- **`/metrics/resource`** implements
  [KEP-2371](https://github.com/kubernetes/enhancements/tree/master/keps/sig-node/2371-cri-pod-container-stats)'s
  small, well-specified metric set completely: `node_cpu_usage_seconds_total`,
  `node_memory_working_set_bytes`, `pod_cpu_usage_seconds_total`,
  `pod_memory_working_set_bytes`, `container_cpu_usage_seconds_total`,
  `container_memory_working_set_bytes`.
- **New node-wide CPU accounting** (`metrics.rs::read_node_cpu_seconds`) —
  parses the aggregate `cpu ` line of `/proc/stat` (same technique
  `node_exporter` uses) to get cumulative node CPU core-seconds since boot.
  This closes the "`/stats/summary` doesn't report node CPU" gap noted in
  round 7 too, for free — `server::stats::node_stats()` still doesn't use it
  (out of scope for this round; `/stats/summary`'s JSON shape wasn't
  touched), but the underlying data now exists for whichever endpoint wants
  it next.
- **`/metrics/cadvisor` is a deliberately scoped-down subset**, not the
  full cAdvisor catalog — real cAdvisor exposes dozens of metrics (network/
  disk I/O, per-cpu-core breakdowns, `container_last_seen`, spec/limit
  metrics, and more) that would be a lot of surface for an edge agent
  that's otherwise deliberately lean, and CRI's own stats don't carry most
  of that data anyway (no network/disk I/O in `ListPodSandboxStats`).
  Implements the four metrics most dashboards/scrapers built against
  cAdvisor actually read: `container_cpu_usage_seconds_total`,
  `container_memory_usage_bytes`, `container_memory_working_set_bytes`,
  `container_memory_rss`. Also drops cAdvisor's usual `id`/`name`/`image`
  labels (container cgroup path, runtime name, image ref) — nothing in
  `PodUsage` tracks those today, and faking them would be worse than
  omitting them; only `namespace`/`pod`/`container` labels are emitted.
- Deleted `deploy/lib/test/cases/unimplemented.sh` — its one remaining
  placeholder test was exactly this gap; replaced by real functional tests
  in the new `prom_metrics.sh` (same treatment streaming.sh/stats.sh got
  when their gaps closed in earlier rounds).

397 tests passing with `--features cri` (up from 374), 168 mock-only (up
from 161 — `read_node_cpu_seconds`'s pure parser lives in `metrics.rs`,
which isn't `cri`-gated).

## Round 9: graceful node shutdown (2026-07-31)

Continued closing gaps ("continue" — no further scoping given). Picked
graceful node shutdown next: it's the one item on the "biggest remaining"
list that's specifically valuable for nodelet's actual target hardware
(edge devices get power-cycled far more often than a datacenter node ever
does), and unlike PVC/CSI or the CPU/Memory/Topology managers it's a
self-contained addition that doesn't touch the container-creation path at
all.

- **New `shutdown.rs`** (`cri`-feature-gated, like `server.rs` and
  `static_pods.rs`'s real uses) — holds a systemd-logind shutdown-delay
  inhibitor lock (`Inhibit("shutdown", "nodelet", ..., "delay")` over
  D-Bus, via the `zbus` crate) for as long as nodelet is running with the
  feature enabled, and subscribes to logind's `PrepareForShutdown` signal.
  On `PrepareForShutdown(true)`, terminates every pod on the node through
  the *same* graceful path a normal delete already gets
  (`PodRuntime::remove_pod` — `preStop` + a bounded `StopContainer`
  timeout), bounded by a fixed time budget, then drops the held fd (closing
  it releases the lock) so shutdown actually proceeds. On `(false)`
  (shutdown cancelled), re-acquires the lock for next time.
- **Priority-ordered, budget-capped** — non-critical pods terminate first;
  `system-node-critical`/`system-cluster-critical` pods (reuses
  `eviction::is_critical`, the same definition node-pressure eviction
  already uses) get their own reserved sub-budget
  (`NODELET_SHUTDOWN_GRACE_PERIOD_CRITICAL_SECS`) and go last, so ordinary
  workloads get first crack at a clean exit while system add-ons keep
  serving as long as possible. Each pod's own `terminationGracePeriodSeconds`
  is capped to whatever's actually left in its group's budget — a pod
  asking for a 5-minute grace period doesn't get it if the node only has 30
  seconds of runtime left.
- **New config**: `NODELET_SHUTDOWN_GRACE_PERIOD_SECS` (default `0`,
  disabled — matches upstream, where this is opt-in) and
  `NODELET_SHUTDOWN_GRACE_PERIOD_CRITICAL_SECS` (default `0`, clamped to
  never exceed the total). `run()` doesn't even connect to D-Bus when
  disabled, so this is a true no-op on hosts without systemd or a system
  bus, same as every other opt-in background loop in this codebase.
- Pure scheduling logic (`split_by_criticality`, `budget_split`,
  `capped_grace_period`) is fully unit tested — 14 new tests. The D-Bus
  glue itself (`Connection::system()`, the `Inhibit` call, the signal
  stream) is **not** — see the caveat below.

374 tests passing with `--features cri` (up from 360), 161 mock-only (this
feature is entirely `cri`-gated, so it adds nothing to the mock-only count).
`deploy/lib/test/cases/graceful_shutdown.sh` added as a manual-note skip
test, not an automated one — see why below.

**Known limitation, honestly flagged, same treatment as round 6's exec/
attach proxy**: the D-Bus interaction was written and compiled against the
`zbus` 5.x API (verified by reading its vendored source directly, since no
network docs were consulted) but has never been run against a real
systemd-logind — there's no system/session D-Bus bus reachable in the
sandbox that built this. The three places most likely to need adjustment
on first real use: (1) whether `Connection::system()` succeeds inside
whatever init/container context nodelet actually runs in (needs
`/run/dbus/system_bus_socket` reachable — likely fine on a real systemd
host, questionable inside a minimal container without the host's D-Bus
socket bind-mounted in), (2) whether the `Inhibit` call is permitted by the
host's polkit policy for whatever user nodelet runs as (typically requires
root, or an explicit policy grant), (3) whether `PrepareForShutdown`'s
signal body actually deserializes as a bare `bool` the way `msg.body()
.deserialize()` expects (this matches the D-Bus signal signature `b`
documented for logind, but wasn't observed on the wire). None of these can
regress anything when the feature is disabled (the default) — worst case
if any of the three is wrong, `run()` logs a warning and returns, same as
today, and shutdown behaves exactly as it did before this feature existed
(SIGKILL-on-power-loss, not preStop-first). Also not implemented: real
kubelet's per-`PriorityClass`-level budget bands (`shutdownGracePeriodByPod
Priority`, a list of arbitrary priority/grace-period pairs) — this uses the
simpler two-tier critical/non-critical split kubelet's own
`--shutdown-grace-period`/`--shutdown-grace-period-critical-pods` flags
predate that with, which is a closer match to nodelet's minimalism anyway.

## Round 8: ephemeral containers (`kubectl debug`) (2026-07-30, same day)

Continued closing gaps ("continue" — no further scoping given). Picked
ephemeral containers next: it reuses the exec/attach proxy infrastructure
from round 6 for actually *using* a debug session, so `kubectl debug -it`
was already half-working — this closes the other half, getting the
container itself created and started.

- **`spec.ephemeralContainers` → CRI containers** — `ensure_pod()` now walks
  `spec.ephemeralContainers` after the app-container loop and starts any not
  already present, via a new `ensure_ephemeral_container()` in
  `runtime/cri.rs`. Unlike app containers, these are **one-shot**: once a
  container with that name exists (running or exited), it's never recreated
  or restarted, regardless of the pod's `restartPolicy` — matches real
  kubelet, which has no notion of "restart a debug session."
- **`EphemeralContainer` → CRI `ContainerConfig`** — reuses the exact same
  `create_and_start_container()` app/init containers go through, via a new
  `ephemeral_to_container()` that maps `EphemeralContainer`'s fields onto the
  regular `Container` shape (they're near-identical; `ports` is dropped,
  matching real kubelet, and `targetContainerName`, process-namespace-sharing
  metadata, is a no-op here since nodelet's sandbox containers already share
  the sandbox's PID namespace).
- **New `CTR_EPHEMERAL_LABEL`** — same pattern as the existing init-container
  label: lets status-building and future GC tell ephemeral containers apart
  from app containers without a second side table. `build_status()` now
  excludes both init- and ephemeral-labeled containers from the app-container
  phase/readiness computation (a debug container exiting must never flip the
  pod to Succeeded/Failed, or gate `ContainersReady`).
- **`PodStatus.ephemeralContainerStatuses`** — new `RuntimeStatus.ephemeral_containers`
  field (mirrors `init_containers`), populated in `pods.rs::build_pod_status`.
  Reported as `Terminated` (not `Waiting`/`PodInitializing`) when not
  running, since an ephemeral container that's stopped is *done*, not
  "still starting up" — the opposite framing init containers need.

360 tests passing with `--features cri` (up from 353), 161 mock-only.
`deploy/lib/test/cases/ephemeral_containers.sh` added — runs
`kubectl debug <pod> --image=... --container=debugger -- sleep 3600` against
a live pod and asserts `ephemeralContainerStatuses` reports it running and
the pod's own phase is untouched; skips cleanly if the test cluster's
kubectl/apiserver doesn't support the `ephemeralcontainers` subresource.

**Known simplification, documented not hidden**: exit codes aren't tracked
for ephemeral containers (`ContainerStateTerminated.exit_code` is always
reported as `0`) — real kubelet fetches this via `ContainerStatus` same as
init containers do, but that's an extra CRI round-trip per ephemeral
container on every status build, only worth paying if something actually
reads it; nothing does yet since `kubectl debug` output goes through
`exec`/`attach`, not the exit code.

## Round 2: correctness gaps closed (user-chosen sequencing)

User picked "correctness gaps first" out of the full list below — these were
"silently wrong" (a pod spec that looks correct behaved differently than on
real Kubernetes), as opposed to "missing feature, fails loudly." All five
are now done, each with unit tests for the pure translation logic
(`runtime/cri_tests/linux_resources.rs`, `linux_security_context.rs`,
`dns_config.rs`, `registry_auth.rs`, `init_container_decision.rs`):

- ✅ **Init containers** — `spec.initContainers` now run to completion, in
  order, before app containers start (`CriRuntime::ensure_init_containers`).
- ✅ **Resource requests/limits** — translated to CRI `LinuxContainerResources`
  (cpu shares from requests, cpu quota/period + memory limit from limits) —
  containers are no longer unbounded regardless of what the Pod spec asks for.
- ✅ **securityContext** — `runAsUser`/`runAsGroup`, `privileged`,
  `readOnlyRootFilesystem`, capabilities add/drop, `allowPrivilegeEscalation`
  → `no_new_privs`, `supplementalGroups`, and `seccompProfile` (RuntimeDefault/
  Unconfined/Localhost) now reach CRI's `LinuxContainerSecurityContext`.
  Still not translated: AppArmor profile, SELinux options, and runAsNonRoot
  *verification* against the image's actual configured user (needs image
  inspection, not just pass-through — left as a follow-up).
- ✅ **DNS config** — `dnsPolicy` (ClusterFirst/Default/None) +
  `dnsConfig` now set CRI's `PodSandboxConfig.dns_config`, via new
  `NODELET_CLUSTER_DNS`/`NODELET_CLUSTER_DOMAIN` config (kubelet's
  `--cluster-dns`/`--cluster-domain` equivalents).
- ✅ **Private registry auth** — `imagePullSecrets` (`kubernetes.io/dockerconfigjson`)
  are now resolved into CRI `AuthConfig` for `PullImageRequest`. Legacy
  `kubernetes.io/dockercfg` (no `"auths"` wrapper) and ServiceAccount-linked
  pull secrets are not handled yet.

215 tests passing with `--features cri` (up from 164), 107 with the default
(mock-only) build. Both builds compile clean.

## Round 3: pod-lifecycle correctness + eviction (2026-07-30, same day)

User said "keep closing the gaps, get to 100%" with no further scoping —
continued in the same "correctness first" vein rather than jumping straight
to the largest single remaining item (the streaming exec/logs/attach/
port-forward server, which needs a whole new TLS-authenticated HTTP(S)
listener and is a project of its own). Closed, each with unit tests:

- **Termination grace period + preStop hook** — `PodRuntime::remove_pod()`
  now takes the full `Pod` (was just `namespace`/`name`) specifically so it
  can read `terminationGracePeriodSeconds` and run each container's
  `preStop` hook before stopping it.
- **postStart lifecycle hook** — runs right after `StartContainer` succeeds.
- **Exit-code-aware `restartPolicy: Never` phase** — `Failed` vs `Succeeded`
  now actually depends on the exit code, not just "did everything exit."
- **Real container restart counts** — was hardcoded `0` since the very
  first pass; now a real per-container counter, also used as CRI's
  `attempt` number so restarted containers don't overwrite their own log file.
- **Projected + downwardAPI volumes** — the two most commonly hit
  "volume type not supported" warnings before this (a projected volume is
  what backs the auto-mounted `kube-api-access-*` service account volume,
  though its `serviceAccountToken` source specifically still isn't — see below).
- **hostAliases + fsGroup** — the two security/identity-adjacent volume
  behaviors that were pure no-ops before.
- **Node-pressure eviction** — the one item `ARCHITECTURE.md` had already
  called out by name as not implemented; now something actually happens
  when a node reports pressure, not just a condition update.

260 tests passing with `--features cri` (up from 215 at the start of this
round), 125 with the default (mock-only) build.

## Round 7: /stats/summary, RuntimeClass, usage-based eviction (2026-07-30, same day)

Continued closing gaps ("continue" — no further scoping given). Picked
these three because they compound: `/stats/summary` needed real per-pod
usage data anyway, and once that existed, feeding it back into eviction's
ranking (previously request-based only, explicitly flagged as a
simplification in round 3) was a small, self-contained follow-on rather
than a new investigation.

- **`/stats/summary`** — discovered CRI already solves the hard part:
  `ListPodSandboxStats` returns real per-pod *and* per-container CPU/memory
  usage in one call, with the runtime (containerd) handling cgroup-path/
  driver differences internally. No cgroup file reading needed at all.
- **Eviction ranking now usage-based** — `eviction.rs`'s tie-break within a
  QoS class uses the same CRI stats when available, falling back to
  requested memory per-pod otherwise (mock runtime, or a too-new pod CRI
  hasn't measured yet).
- **RuntimeClass** — `spec.runtimeClassName` now resolves to CRI's
  `runtime_handler` (was hardcoded empty/default before), so gVisor/Kata/
  etc. selection actually works.

353 tests passing with `--features cri` (up from 345), 157 mock-only.
`deploy/lib/test/cases/stats.sh` and `runtime_class.sh` added for live
validation — the RuntimeClass test only proves the *lookup and wiring*
using whatever handler this containerd already knows about (commonly
`runc`), since alternative-runtime binaries aren't something this suite
can assume are installed.

## Round 6: the kubelet HTTP(S) server — exec/logs/attach/port-forward (2026-07-30, same day)

User said "finish closing the gaps" — this was the one deliberately deferred
in round 4 as needing its own dedicated pass ("a project of its own: TLS,
auth, the whole listener"). Built it:

- `crates/nodelet/src/server/` (new, `cri`-feature-gated): `tls.rs`
  (self-signed cert via `rcgen`, cached as DER), `auth.rs` (bearer token
  via `TokenReview`), `routes.rs` (path/query parsing + dispatch), `logs.rs`
  (`kubectl logs`, including `-f`), `exec.rs` (`kubectl exec`/`attach`/
  `port-forward`, proxied to the CRI runtime's own streaming server rather
  than reimplementing SPDY/WebSocket).
- New `PodRuntime` trait methods (`container_log_path`, `exec_url`,
  `attach_url`, `port_forward_url`) implemented in `cri.rs` against CRI's
  `ContainerStatus`/`Exec`/`Attach`/`PortForward` RPCs.
- `Node.status.daemonEndpoints.kubeletEndpoint.port` — never set before;
  without it the apiserver has nowhere to proxy exec/logs requests to
  regardless of whether a server exists.
- New dependencies (all `cri`-gated): `rcgen`, `hyper`+`hyper-util` server
  features, `http-body-util`, `tokio-rustls`, `percent-encoding`,
  `tokio-stream`.
- Everything logic-based has real unit tests: CRI log-line parsing/
  reassembly, path/query routing, bearer token extraction, and (genuinely
  integration, not mocked) TLS cert generation/caching/permissions against
  the real filesystem. 345 tests passing with `--features cri` (up from
  302), 155 mock-only.
- **Honest confidence note**: the connection-splicing proxy in `exec.rs`
  (dial the CRI-returned URL, replay the client's upgrade request, mirror
  the response, `copy_bidirectional` the two upgraded connections) was
  written as carefully as reasoning allows but never observed completing a
  real SPDY/WebSocket handshake — this sandbox has no live cluster to test
  against. `deploy/lib/test/cases/streaming.sh` exists specifically to
  prove or disprove this the first time it runs for real; treat `kubectl
  exec` as the most likely thing in this round to need a live-cluster fix.
  `kubectl logs` (no protocol upgrade involved, just an HTTP response body)
  carries much higher confidence.
- Still explicitly out: `/stats/summary` (no usage-stats collector),
  client-cert auth (bearer token only), real `SubjectAccessReview`
  authorization (currently `AlwaysAllow` once a token authenticates,
  matching kubelet's own historical default).

## Round 5: live-cluster e2e test suite + initContainerStatuses fix (2026-07-30, same day)

User asked for two things: keep writing Rust tests for whatever's testable
that way, and — for what genuinely isn't (this is a live-container-runtime
project; a lot of correctness can only really be proven against a real
apiserver + real containerd) — a bash integration-test suite, structured
like `deploy/bootstrap-source.sh`'s `lib/*.sh` module pattern, that the user
runs manually against a real k3s deployment.

- **Found and fixed while building the suite**: `PodStatus.initContainerStatuses`
  and the `Initialized` condition were never populated at all —
  `kubectl describe`'s `Init:N/M` display had nothing to read, and
  `Initialized` always reported `True` even while genuinely waiting on init
  containers. New `RuntimeStatus.init_containers`/`.initialized` fields,
  threaded through `mock.rs`/`cri.rs`/`pods.rs`, with new unit tests.
- **`deploy/test-e2e.sh` + `deploy/lib/test/`**: a harness (register/run/
  assert, PASS/FAIL/SKIP reporting), kubectl wait/get helpers, and one case
  file per feature area, covering nearly everything from rounds 1–4 against
  a real cluster — pod lifecycle, init container ordering (structural, not
  just status-string), crash-restart + restart counts, exit-code-aware
  `Never` phase, all three probe types (including a real `httpGet` against
  a real pod IP), postStart/preStop hooks, termination grace period,
  ConfigMap/Secret/downwardAPI/projected volumes, real `serviceAccountToken`
  minting, hostAliases, fsGroup, `runAsUser`/`readOnlyRootFilesystem`,
  custom DNS config, **resource limits actually enforced in the container's
  own cgroup v2 files** (not just translated correctly in isolation), node
  status/pressure conditions, image GC, static pods + mirror pods, log
  rotation, and — deliberately — active assertions that `kubectl exec`/
  `kubectl logs` still *don't* work, so this suite fails loudly instead of
  going silently stale the moment someone lands the streaming server.
- The key trick making most of this possible without `kubectl exec`/`logs`:
  single-node architecture means the test script runs on the same host as
  nodelet, so a container's self-check output written into a shared
  `emptyDir` — or nodelet's own materialized ConfigMap/Secret/downwardAPI/
  projected volume — is directly readable off the host filesystem at the
  exact path bind-mounted into the container.
- Deliberately **not** automated (documented as manual procedures instead):
  node-pressure eviction and orphaned-sandbox GC, since exercising either
  needs exhausting a real resource or stopping nodelet out from under a pod
  — not something to do automatically to a host/service someone's relying on.

## Round 4: PID pressure, log rotation, static/mirror pods, serviceAccountToken (2026-07-30, same day)

User said "you pick the path... let's get this finished" — picked verifiable,
self-contained gaps over the streaming exec/logs/attach/port-forward server,
which is large enough to be a project of its own (TLS, auth, a whole new
listener) and can't be validated here without a live cluster to test
against. Explicitly did **not** attempt that this round; see below.

- **Real PID pressure** — was the one pressure signal still hardcoded
  `False` after rounds 1–3 fixed memory/disk; now real, same pattern.
- **Container log rotation** — `--container-log-max-size`/`-max-files`
  equivalent; previously logs grew forever.
- **Static pods + mirror pods** — the big win here is architectural, not
  just code volume: static pods reuse the exact same `PodRuntime` normal
  apiserver-sourced pods do, so every correctness fix from rounds 1–3
  (resource limits, securityContext, probes, volumes, ...) applies to static
  pods for free. Disabled by default (`NODELET_STATIC_POD_PATH` unset),
  matching upstream.
- **serviceAccountToken projected volume** — checked kube-rs 4.0 first (no
  typed helper for the `TokenRequest` subresource; used `kube::Client::request`
  with a raw HTTP call instead). Real apiserver-signed tokens, not a stub —
  this is what every actual `kube-api-access-*` volume needs to let a pod
  authenticate back to the apiserver, previously the one skipped source in
  an otherwise-working projected volume.

297 tests passing with `--features cri` (up from 260 at the start of this
round), 150 with the default (mock-only) build.

## Full kubelet responsibility list vs. current nodelet state

Legend: ✅ done · 🟡 partial · ❌ missing

### Pod & container lifecycle
- ✅ Pod sandbox + container create/start/stop/remove via CRI
- ✅ Restart-on-exit honoring `restartPolicy`
- ✅ **Init containers** — run to completion, in order, before app containers (`ensure_init_containers`)
- ✅ **Ephemeral containers** (`kubectl debug`) — `spec.ephemeralContainers` started once (never restarted) via `ensure_ephemeral_container()`, reported in `PodStatus.ephemeralContainerStatuses`, excluded from pod phase/readiness. Exit codes not tracked (always reported `0`) — see Round 8 notes.
- ✅ **postStart / preStop lifecycle hooks** (`exec`/`httpGet`/`sleep`; not `tcpSocket`) — `run_lifecycle_hook()`. A failing `postStart` is logged, not (yet) turned into a container kill+restart like real kubelet does.
- ✅ **Termination grace period** — `terminationGracePeriodSeconds` now drives `preStop` + a per-container `StopContainer` timeout before `StopPodSandbox` (`graceful_stop_containers()`), instead of an untimed sandbox stop.
- ✅ **Container restart count** — real per-container counter (`restart_count_from`/`bump_restart_count_in`), threaded through `ContainerConfig.metadata.attempt` too (so restarted containers get distinct log files, not overwritten ones).
- ✅ **Exit-code-aware phase computation** — `restartPolicy: Never` now reports `Failed` (not `Succeeded`) when a container exited nonzero (`compute_phase()`'s new `any_failed` parameter).

### Resource management
- ✅ **Container resource requests/limits** — translated to CRI `LinuxContainerResources` (cpu shares/quota/period, memory limit; `linux_resources()`)
- ✅ **QoS cgroup hierarchy** (`--cgroups-per-qos`) — every pod sandbox now gets a `cgroup_parent` scoped by QoS class (`cgroup.rs::cgroup_parent_for`): `/kubepods/pod<uid>` for Guaranteed, `/kubepods/burstable/pod<uid>` / `/kubepods/besteffort/pod<uid>` otherwise, wired into `runtime/cri.rs::ensure_pod`.
- ✅ **cgroup driver — no detection needed.** CRI's own `LinuxPodSandboxConfig.cgroup_parent` proto contract specifies the cgroupfs-style syntax is always sent, with "the container runtime can convert it to systemd semantics if needed" — nodelet always builds the cgroupfs-style path and lets the runtime do any systemd-unit-naming conversion, matching that contract exactly. No `--cgroup-driver`-equivalent config needed.
- ✅ **Node allocatable enforcement** (`--enforce-node-allocatable=pods`, its own upstream default) — `cgroup.rs::enforce_node_allocatable`, called once at startup, creates and caps the top-level `kubepods` cgroup (cpu.max/memory.max) at `Node.status.allocatable` so pods collectively can never exceed it. `Node.status.allocatable` itself is now `capacity - (system-reserved + kube-reserved)` (`node.rs::allocatable_map`) rather than always equal to capacity — a real correctness fix, not just the enforcement mechanism (`NODELET_SYSTEM_RESERVED_CPU_MILLICORES`/`_MEMORY_BYTES`, `NODELET_KUBE_RESERVED_CPU_MILLICORES`/`_MEMORY_BYTES`, all default `0`). Best-effort: needs root + cgroup v2 (cgroup v1 unsupported, matching modern kubelet defaults), logs and continues on failure rather than blocking startup — **unvalidated against a real cgroup v2 hierarchy**, no writable `/sys/fs/cgroup` in the sandbox that built this; see `deploy/lib/test/cases/cgroup_hierarchy.sh` for the live-cluster check.
- ✅ **CPU Manager** (`cpu_manager.rs`, `static` policy) — Guaranteed-QoS containers requesting a whole number of CPUs get pinned to exclusive cores (`cpuset_cpus`); every other container gets the current shared pool (total minus reserved minus exclusively-claimed). Opt-in (`NODELET_CPU_MANAGER_POLICY`, default `none` matching upstream). **Bidirectional as of round 16**: `runtime/cri.rs::refresh_shared_pool_cpusets()` retroactively shrinks/grows every already-running shared-pool container's `cpuset_cpus` via CRI's `UpdateContainerResources` whenever an exclusive claim is made or released, matching real kubelet's own policy behavior — not just newly-created containers. As of round 17, NUMA-aware when Topology Manager is also enabled (`allocate_preferring()`); simple ascending-CPU-ID selection otherwise.
- ✅ **Memory Manager** (`memory_manager.rs`, `static` policy) — Guaranteed-QoS containers with a memory limit get pinned to a single NUMA node (`cpuset_mems`). Opt-in (`NODELET_MEMORY_MANAGER_POLICY`, default `none`). **First-slice scope**: never spans multiple NUMA nodes (falls back to unconstrained if no single node has enough free capacity, rather than upstream's multi-node spanning); no shared-pool tracking or `UpdateContainerResources` retroactive sweep for non-pinned containers (unlike CPU Manager) — they're simply left unconstrained; no per-NUMA-node `--reserved-memory`-equivalent reservation. See round 18 notes.
- ✅ **Topology Manager** (`topology.rs`) — coordinates CPU Manager, Memory Manager (as of round 18), and device plugins so a container's exclusive cores, pinned memory, and allocated devices land on the same NUMA node. Opt-in (`NODELET_TOPOLOGY_MANAGER_POLICY`, default `none`); `best-effort` prefers alignment without rejecting, `restricted`/`single-numa-node` reject the container if no single NUMA node satisfies every hint provider. **Single-node-only algorithm, not upstream's full bitmask/permutation one** — reaches the same answer whenever one NUMA node can satisfy everything (the common case), but won't find a valid multi-node alignment upstream's `restricted` would accept; this treats `restricted` identically to `single-numa-node`. See round 17/18 notes.
- 🟡 **Device plugins** (`device_plugins.rs`, GPU/FPGA/etc. hardware resources) — discovered via the same dynamic plugin-registration protocol CSI drivers use (`plugin_registry.rs`, round 13); tracks live device inventory/health per resource name via `ListAndWatch`, advertises healthy counts on `Node.status.capacity`/`.allocatable`, and calls `Allocate()` during container creation to inject the envs/mounts/device-nodes/annotations a plugin returns. Not implemented: `GetPreferredAllocation` (always does its own first-healthy-unallocated pick) and `PreStartContainer`. Unvalidated against a real device plugin — see round 14 notes.
- ❌ In-place pod resource resize (newer, still-maturing upstream feature)

### Security context
- ✅ **`securityContext`** — `runAsUser`/`runAsGroup`, capabilities add/drop, `privileged`, `readOnlyRootFilesystem`, `allowPrivilegeEscalation`→`no_new_privs`, `supplementalGroups`, `seccompProfile` (`linux_security_context()`). Not yet: `runAsNonRoot` verification against the image's actual user, AppArmor profile, SELinux options.
- ❌ Pod-level `sysctls`
- ✅ **`fsGroup` volume ownership application** — recursive chown + setgid on every volume directory nodelet itself materializes (`apply_fs_group()`). Only reaches those (ConfigMap/Secret/emptyDir/downwardAPI/projected) — there's no real PV/hostPath for it to reach beyond that yet.
- ✅ **RuntimeClass** — `spec.runtimeClassName` resolves the cluster-scoped `RuntimeClass` object and passes its `.handler` through as CRI's `runtime_handler` (`resolve_runtime_handler()`), so gVisor/Kata/etc. selection actually reaches the runtime. `Overhead.podFixed` now also accounted: converted to `LinuxContainerResources` (`resource_list_to_linux_resources()`) and set on `LinuxPodSandboxConfig.overhead`, closed alongside round 11's cgroup work since it's the same struct/code path. A missing/invalid RuntimeClass still isn't rejected at admission (falls back to the default handler with a warning instead, since nodelet can't enforce the validation a real cluster's admission plugin normally would).

### Networking
- ✅ **DNS config** — `dnsPolicy`/`dnsConfig` → CRI `PodSandboxConfig.dns_config` (`dns_config_for()`), via new `NODELET_CLUSTER_DNS`/`NODELET_CLUSTER_DOMAIN`
- ✅ **`hostAliases`** — generates a pod-specific `/etc/hosts` (`write_etc_hosts()`) and bind-mounts it in, exactly how real kubelet does it (CRI has no dedicated field for this)
- ✅ Service/ClusterIP/NodePort routing (nftables — pre-existing, kube-proxy's job but already reimplemented here)

### Images
- ✅ **Private registry auth** — `imagePullSecrets` (`kubernetes.io/dockerconfigjson`) → CRI `AuthConfig` (`resolve_pull_auth()`). Not yet: legacy `kubernetes.io/dockercfg`, ServiceAccount-linked pull secrets, credential-provider exec plugins.
- 🟡 Image garbage collection — unreferenced-image sweep exists but not the real kubelet policy (disk-pressure-triggered high/low watermark GC, `--image-gc-high-threshold`/`--image-gc-low-threshold`)
- ✅ **Container log rotation** — running containers' log files are rotated past `NODELET_CONTAINER_LOG_MAX_SIZE_BYTES`, keeping `NODELET_CONTAINER_LOG_MAX_FILES` (`rotate_log_file()` + CRI `ReopenContainerLog`)

### Volumes
- ✅ ConfigMap / Secret / emptyDir (materialized to host paths)
- ✅ **Projected volumes** — `configMap`/`secret`/`downwardAPI`, and now `serviceAccountToken` too (mints a real token via the `TokenRequest` API — `resolve_service_account_token()`; needs nodelet's client to have `create` on `serviceaccounts/token` in the namespace, a real RBAC requirement) merge into the volume dir, with `items`/`KeyToPath` key-selection-and-rename support. `clusterTrustBundle` sources are still skipped with a warning.
- ✅ **downwardAPI volumes** (`write_downward_api_volume()`, `fieldRef` only — `resourceFieldRef` needs the resolved container spec and isn't supported)
- 🟡 **PersistentVolumeClaim / CSI** (`runtime/csi.rs`, `plugin_registry.rs`) — resolves a bound PVC's `PersistentVolume.spec.csi` source and drives `NodeStageVolume` (if the driver supports it)/`NodePublishVolume`, with per-node reference counting so `NodeUnstageVolume` only fires once every pod using a volume is gone. **Dynamic CSI driver discovery** (round 13) — a driver's `node-driver-registrar` sidecar can register itself against `NODELET_PLUGIN_REGISTRY_PATH` the same protocol it'd use against real kubelet's plugin watcher, no static config needed; `NODELET_CSI_DRIVERS` still works too, as a seed/override. **`nodeStageSecretRef`/`nodePublishSecretRef`** (round 13) — resolved to real Secret data and passed through to the driver. **Attach coordination** (round 19) — checks `CSIDriver.spec.attachRequired`, and for drivers that need it, waits on the matching `VolumeAttachment.status.attached` before Stage/Publish, threading `status.attachmentMetadata` through as `publish_context`. Calling the Controller service itself (`ControllerPublishVolume`/`ControllerUnpublishVolume`) stays out of scope — that's external-attacher's job upstream too, not kubelet's, confirmed against docs in round 19. Still out of scope: device-plugin registrations against this module (the same registration protocol, explicitly rejected with a real `NotifyRegistrationStatus{plugin_registered: false}` rather than ignored — that's `device_plugins.rs`'s job instead). Unvalidated against a real CSI driver — see rounds 12, 13, and 19 notes.
- ❌ hostPath (explicitly unsupported today, logged and dropped)
- ❌ `emptyDir.sizeLimit` enforcement
- ❌ subPath `$(VAR)` expansion

### Node-pressure eviction
- ✅ MemoryPressure/DiskPressure *conditions* reflect real reads
- 🟡 **Eviction** — `eviction_loop()` now acts on real pressure: ranks eligible pods by QoS class (`eviction.rs`'s `qos_class()`/`pick_eviction_candidate()` — BestEffort before Burstable, Guaranteed and `system-*-critical` pods never evicted), evicts one per check. Ranking within a QoS class now uses **real memory usage** from CRI's `ListPodSandboxStats` (the same source `/stats/summary` uses) when known, falling back to requested memory otherwise (`eviction_weight()`). Still simplified vs. real kubelet: no soft-threshold grace period (hard-style immediate action only).
- ✅ PID pressure — real `/proc/sys/kernel/pid_max` + a `/proc` scan (`read_pid_info()`/`pid_pressure()`), same fail-open pattern as memory/disk

### Static pods & mirror pods
- ✅ **Static pod manifest directory watching** (`NODELET_STATIC_POD_PATH`, disabled by default like real kubelet's optional `staticPodPath`) — `static_pods::run()` scans, hashes to detect changes, and drives the same `PodRuntime` normal pods use (so resource limits/securityContext/volumes/probes all apply identically)
- ✅ **Mirror pod creation/reconciliation** — a read-only Pod object per static pod (`kubernetes.io/config.mirror`/`kubernetes.io/config.source: file` annotations, matching real kubelet's markers), deleted when the manifest disappears. Simplified vs. real kubelet: no exact hash-based drift-detection annotation value (nodelet's own file-content hash serves the same "did it change" purpose internally, just isn't exposed as that specific annotation).

### kubelet HTTP(S) server (`crates/nodelet/src/server/`, `cri` feature only)
- ✅ **`kubectl logs`** (`server::logs`) — parses containerd's CRI log file format back into raw output, with `follow`/`tailLines`/`sinceTime`/`timestamps`/`previous` query params. `follow` mode polls the file for growth rather than using inotify (matches the poll-based style everywhere else in nodelet — probes.rs, gc.rs).
- ✅ **Streaming exec/attach/port-forward** (`server::exec`) — CRI's actual model here: `Exec`/`Attach`/`PortForward` RPCs return a one-shot URL to the *runtime's own* streaming server (containerd runs one internally, typically on `127.0.0.1:<random-port>` — unreachable to a remote kubectl client directly). nodelet doesn't implement the SPDY/WebSocket protocol itself; it dials that URL, replays the client's upgrade request, mirrors the response, and once both sides upgrade, splices the two raw connections together (`tokio::io::copy_bidirectional`) — the same "proxy" pattern real kubelet uses. **This is the piece with the least confidence without a live cluster**: the request/response replay and connection splicing were written as carefully as reasoning allows, but an actual SPDY/WebSocket handshake end-to-end was never observed — `deploy/lib/test/cases/streaming.sh` exists specifically to prove (or disprove) this for real.
- ✅ TLS serving certificate — self-signed, generated on first start via `rcgen` and cached as raw DER under `NODELET_SERVER_CERT_DIR` (persists across restarts so a client that already trusts it doesn't get invalidated). Not yet: CSR-based issuance against a real cluster CA.
- ✅ Bearer token authentication via `TokenReview` (the same mechanism real kubelet's `--authentication-token-webhook` uses). Authorization is deliberately `AlwaysAllow` once a token authenticates — matches real kubelet's own historical default (`--authorization-mode=AlwaysAllow`), not a from-scratch `SubjectAccessReview` implementation. No anonymous access (real kubelet has historically defaulted to allowing it; nodelet doesn't).
- ✅ `Node.status.daemonEndpoints.kubeletEndpoint.port` now advertised (was never set before — without it the apiserver has no route to proxy exec/logs/attach/port-forward requests to at all, regardless of whether a server is listening).
- ✅ **`/stats/summary`** (`server::stats`) — built from CRI's `ListPodSandboxStats` (one call gets per-pod *and* per-container CPU/memory usage, no cgroup-path guessing needed). Real caveat, not a nodelet limitation: `kubectl top` itself needs metrics-server (or another `metrics.k8s.io` implementation) deployed and configured to scrape this — implementing the endpoint is necessary but not sufficient for `kubectl top` on its own. Node-level CPU usage isn't populated in this endpoint's JSON shape (unlike `/metrics/resource` below, which does report it) — only memory comes from `/proc/meminfo` here.
- ✅ **`/metrics/resource`** (`server::prom_metrics`) — full [KEP-2371](https://github.com/kubernetes/enhancements/tree/master/keps/sig-node/2371-cri-pod-container-stats) metric set, including real node-wide CPU usage from a new `/proc/stat` parser (`metrics.rs::read_node_cpu_seconds`).
- 🟡 **`/metrics/cadvisor`** (`server::prom_metrics`) — a deliberately scoped-down subset of real cAdvisor's much larger legacy catalog: `container_cpu_usage_seconds_total`, `container_memory_usage_bytes`, `container_memory_working_set_bytes`, `container_memory_rss`, labeled `{namespace,pod,container}` only (no `id`/`name`/`image` — not tracked in `PodUsage`). Missing: network/disk I/O, per-cpu-core breakdowns, `container_last_seen`, spec/limit metrics.
- ❌ Client certificate authentication (bearer token only)

### Node shutdown
- ✅ **Graceful node shutdown** (`shutdown.rs`) — a systemd-logind shutdown-delay inhibitor lock held via D-Bus, released once every pod's been driven through the normal `preStop`/`StopContainer` teardown path within a configurable time budget (`NODELET_SHUTDOWN_GRACE_PERIOD_SECS`, `0`/disabled by default matching upstream). Non-critical pods terminated first, `system-node-critical`/`system-cluster-critical` pods last, each pod's own `terminationGracePeriodSeconds` capped to whatever's left of the budget. **The D-Bus glue is unvalidated against a real systemd-logind** — no system bus in the environment that built it; see the round 9 notes below and `deploy/lib/test/cases/graceful_shutdown.sh`'s manual spot-check procedure.

### Bootstrapping / config
- ❌ TLS bootstrap (CSR-based initial client cert issuance) — nodelet currently expects to be handed a working kubeconfig directly; lower priority given the project's already-simplified config philosophy, but a real gap if "100%" includes it.
- ❌ `--config` file / drop-in config directory (nodelet uses env vars only) — same caveat as above.

## Scale reality check

The items marked ❌ above are, in aggregate, most of what a real kubelet is —
this is not a "few features," it's multiple person-months of work in upstream
Kubernetes (cgroup management, a TLS-authenticated streaming server, a CSI
client, an eviction manager, security-context translation...). Some are far
higher-value/correctness-critical than others:

- **Correctness-critical, silently wrong today**: resource limits not
  enforced, security context ignored, init containers skipped, DNS not
  configured, private images unpullable. These mean a pod spec that *looks*
  correct produces materially different (and less safe) behavior than on
  real Kubernetes.
- **Missing features, fails loudly/obviously**: `kubectl exec`/`logs`/`top`,
  static pods, PVC/CSI, RuntimeClass.
- **Advanced/opt-in on real clusters too**: CPU/Memory/Topology managers,
  device plugins, in-place resize — most real clusters don't enable these
  either.

## Progress on the original 3-gap pass (completed, commit `fdf003b`)
- [x] Probes (liveness/readiness/startup)
- [x] Pressure metrics (real MemoryPressure/DiskPressure)
- [x] GC (orphaned sandboxes + unreferenced images)

## Progress on full-parity pass (this rescoping)
- [x] Verify scope boundary against kubernetes.io docs
- [x] Comprehensive gap list (this doc)
- [x] Round 2 (user-chosen "correctness gaps first"): init containers,
      resource limits, securityContext, DNS config, private registry auth
- [x] Round 3: termination grace + preStop/postStart hooks, exit-code-aware
      phase, real restart counts, projected/downwardAPI volumes,
      hostAliases, fsGroup, node-pressure eviction
- [x] Round 4: real PID pressure, container log rotation, static/mirror
      pods, serviceAccountToken minting via TokenRequest
- [x] Round 5: live-cluster e2e bash test suite (deploy/test-e2e.sh) +
      initContainerStatuses/Initialized condition fix
- [x] Round 6: kubelet HTTP(S) server — kubectl logs/exec/attach/
      port-forward, TLS, TokenReview auth, daemonEndpoints advertisement.
      **Needs live-cluster validation** — see streaming.sh and this round's
      confidence note above, especially for kubectl exec.
- [x] Round 7: `/stats/summary`, usage-based eviction ranking, RuntimeClass
- [x] Round 8: ephemeral containers (`kubectl debug`)
- [x] Round 9: graceful node shutdown (systemd-logind inhibitor lock,
      unvalidated against a real logind — see the confidence note above)
- [x] Round 10: `/metrics/resource` (complete) + `/metrics/cadvisor`
      (scoped-down subset — see round 10 notes)
- [x] Round 11: cgroup/QoS hierarchy + node allocatable enforcement +
      RuntimeClass Overhead (user-picked over PVC/CSI and the CPU/Memory/
      Topology managers — see round 11 notes for the cgroup-write
      confidence caveat)
- [x] Round 12: PersistentVolumeClaim/CSI, first slice (static driver
      config, no Controller service — see round 12 notes; unvalidated
      against a real CSI driver, same confidence caveat pattern as rounds
      9 and 11)
- [x] Round 13: dynamic CSI driver discovery (plugin_registry.rs) +
      nodeStageSecretRef/nodePublishSecretRef — see round 13 notes, same
      unvalidated-against-a-real-driver caveat as round 12
- [x] Round 14: device plugins (device_plugins.rs) — discovery,
      Node.status.capacity/allocatable advertisement, Allocate() wired into
      container creation. See round 14 notes; same unvalidated-against-
      real-hardware caveat as rounds 12/13's driver-dependent pieces.
- [x] Round 15: CPU Manager static policy (cpu_manager.rs) — first slice,
      no retroactive UpdateContainerResources sweep, no topology/socket
      awareness. See round 15 notes.
- [x] Round 16: CPU Manager retroactive shared-pool updates
      (refresh_shared_pool_cpusets()) — closes round 15's biggest flagged
      gap; not topology/socket-aware yet. See round 16 notes.
- [x] Round 17: Topology Manager (topology.rs) — single-node-only
      alignment algorithm, coordinates CPU Manager + device plugins; no
      Memory Manager to also coordinate. See round 17 notes.
- [x] Round 18: Memory Manager (memory_manager.rs) — single-node-only
      pinning, no shared-pool retroactive tracking, no per-node reserved-
      memory. CPU/Memory/Topology manager thread now closed. See round 18
      notes.
- [x] Round 19: CSI attach coordination — `CSIDriver.spec.attachRequired`
      check + waiting on the matching `VolumeAttachment.status.attached`
      before Stage/Publish, threading `status.attachmentMetadata` through
      as `publish_context`. Calling the Controller service itself
      (`ControllerPublishVolume`/`ControllerUnpublishVolume`) stays out of
      scope — confirmed against docs that's external-attacher's job, not
      kubelet's. See round 19 notes.
- [ ] Everything else in the responsibility list above — biggest remaining
      single items: Topology Manager's multi-node `restricted` allowance,
      device plugin GetPreferredAllocation/PreStartContainer. Ask before
      starting the next round.
