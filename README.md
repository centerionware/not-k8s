# not-k8s

**A single-device Kubernetes that doesn't melt your battery.**

`not-k8s` makes one edge device (a phone, an SBC, a tiny VM) behave like a
self-contained, low-overhead single-node Kubernetes cluster that accepts ordinary
`kubectl apply` and CRDs, runs workloads **offline**, and can later federate to an
upstream cluster over a mesh (Tailscale/Netbird).

The idea, in one sentence: **keep a real (stripped) Kubernetes control plane for 1:1
kubectl/CRD compatibility, and replace only the heavy node agent (the kubelet) with a
lean, event-driven Rust binary** — because that's where the idle CPU/RAM actually goes.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full rationale.
The project's current goal is **100% kubelet feature parity while keeping
this performance advantage** — nodelet is meant to be a genuine drop-in
kubelet replacement, not just a single-node-only tool, even though a
single edge device is where its low-idle-CPU design shines brightest. See
[`docs/GAP_CLOSURE.md`](docs/GAP_CLOSURE.md) for the live, verified
checklist of what's done vs. still missing.

## Quick start

One command, on any Linux box, any architecture — installs everything (see
[Try it in one command](#try-it-in-one-command) below for what that means):

```bash
git clone https://github.com/centerionware/not-k8s && cd not-k8s && ./deploy/bootstrap-source.sh --with-cri
```

---

## Why

K3s/k0s/k8s idle at ~15% RAM and 30–50% of a CPU core on a phone — not because they
do *much*, but because they do it *constantly*: PLEG relists every container every
second, cAdvisor walks every cgroup forever, leases renew every 2s, informers
periodically re-list the world, and several processes each keep their own watch
caches. That's **architecture**, not language. `not-k8s` keeps the control-plane API
surface and rebuilds the node side to be edge-triggered: no PLEG, no cAdvisor
housekeeping, lease heartbeat decoupled from (infrequent) status pushes, one process,
one watch.

## What's here

```
crates/nodelet/        The node agent (Rust). Registers a Node, heartbeats a Lease,
                       watches Pods bound to it, runs them via a pluggable runtime.
  src/config.rs        Env-driven configuration.
  src/node.rs          Node registration + Lease heartbeat + node-status push.
  src/pods.rs          The reconcile loop (apiserver watch ⨉ runtime events).
  src/probes.rs        Liveness/readiness/startup probe execution (opt-in per pod).
  src/metrics.rs        Real MemoryPressure/DiskPressure from /proc + statvfs.
  src/gc.rs            Orphaned-sandbox + unreferenced-image garbage collection.
  src/eviction.rs      Node-pressure eviction: QoS-ranked pod selection.
  src/cgroup.rs        QoS-scoped cgroup_parent + node allocatable enforcement (feature `cri`).
  src/static_pods.rs   Static pod manifest directory + mirror pod reconciliation.
  src/server/          kubelet-style HTTP(S) server: logs/exec/attach/port-forward/stats/metrics (feature `cri`).
  src/shutdown.rs      Graceful node shutdown: systemd-logind inhibitor lock + pod drain (feature `cri`).
  src/runtime/mock.rs  In-memory runtime: reports pods Running, zero engine overhead.
  src/runtime/csi.rs   Minimal CSI Node-service client for PersistentVolumeClaim
                       volumes (feature `cri`).
  src/plugin_registry.rs  Dynamic CSI driver / device plugin discovery: the client side
                       of the CSI/DevicePlugin plugin-registration protocol (feature `cri`).
  src/device_plugins.rs  Device Plugin API client: inventory, Node capacity
                       advertisement, and Allocate() (feature `cri`).
  src/cpu_manager.rs   CPU Manager static policy: exclusive core pinning for
                       Guaranteed-QoS containers (feature `cri`).
  src/memory_manager.rs  Memory Manager static policy: NUMA memory pinning for
                       Guaranteed-QoS containers (feature `cri`).
  src/topology.rs      Topology Manager: NUMA-aware coordination between CPU
                       Manager, Memory Manager, and device plugins (feature `cri`).
  src/userns.rs        User namespace allocation for spec.hostUsers: false
                       (feature `cri`).
  src/runtime/cri.rs   Real containerd/CRI runtime (feature `cri`): pod/init
                       container lifecycle, resource limits, securityContext,
                       DNS, private registry auth.
  proto/cri.proto      Upstream CRI v1 protobuf (vendored).
deploy/                Control-plane setup, launcher, measurement, demo manifests.
  test-e2e.sh          Functional tests against a live cluster — see "Testing" below.
  lib/test/            Its harness/kubectl helpers + one case file per feature area.
docs/ARCHITECTURE.md   Design, trade-offs, roadmap.
```

## Status

Early prototype. The control loop, node registration, Lease heartbeat, and both
runtimes compile and run. The `mock` runtime is fully exercisable today. The `cri`
runtime is **validated end-to-end against real containerd** (1.6.20): it creates a
sandbox, pulls an image, creates and starts a container under runc, reports status,
is idempotent, and tears down cleanly — see [Validate against containerd](#validate-against-containerd).
Not production-ready. No HA, no multi-node scheduling — by design, since those
are the scheduler/etcd/controller-manager's job, not kubelet's (see *Out of
scope* in the architecture doc, verified against upstream docs in
`docs/GAP_CLOSURE.md`).

Working today beyond the basics: liveness/readiness/startup probes, real
node pressure conditions (memory, disk, **and PID**), GC, init containers,
container resource requests/limits (CPU shares/quota, memory limits —
actually enforced via cgroups now, not just advertised), `securityContext`
(runAsUser/Group, capabilities, privileged, readOnlyRootFilesystem,
seccomp), pod DNS (`dnsPolicy`/`dnsConfig`), private registry image pulls
(`imagePullSecrets`), postStart/preStop lifecycle hooks + graceful
termination (`terminationGracePeriodSeconds`), real per-container restart
counts, projected volumes (including a real, apiserver-signed
`serviceAccountToken` via the TokenRequest API) + downwardAPI volumes,
`hostAliases`, `fsGroup`, container log rotation, node-pressure eviction
(evicts one BestEffort/Burstable pod at a time when any pressure condition
is active), **static pods + mirror pods**
(`NODELET_STATIC_POD_PATH`, disabled by default), `kubectl logs`/`kubectl
exec`/`attach`/`port-forward` via a real kubelet-style HTTP(S) server
(`crates/nodelet/src/server/`, TLS + `TokenReview` auth) — **the piece
least validated without a live cluster**, see the "Testing" section below
and `docs/GAP_CLOSURE.md`'s round 6 notes — `/stats/summary` (built from
CRI's own per-pod/per-container usage stats; `kubectl top` also needs
metrics-server deployed separately to actually scrape it), eviction now
ranked by real memory usage instead of just requests, and `RuntimeClass`
(`spec.runtimeClassName` reaches CRI's `runtime_handler` for gVisor/Kata/
etc.), **ephemeral containers** (`kubectl debug`) — one-shot,
never restarted, reported in `PodStatus.ephemeralContainerStatuses`,
excluded from pod phase/readiness — and **graceful node shutdown**
(`NODELET_SHUTDOWN_GRACE_PERIOD_SECS`, disabled by default): holds a
systemd-logind inhibitor lock and drains pods (non-critical first) within a
time budget before letting the host actually power off — **the D-Bus glue
is unvalidated against a real systemd-logind**, see `docs/GAP_CLOSURE.md`'s
round 9 notes. Also `/metrics/resource` (complete) and `/metrics/cadvisor`
(a scoped-down subset — see round 10 notes), the Prometheus-text
alternatives to `/stats/summary`, and a **QoS-scoped cgroup hierarchy +
node allocatable enforcement** — every pod sandbox now gets a
`cgroup_parent` scoped by QoS class, and the top-level `kubepods` cgroup is
created/capped at `Node.status.allocatable` (`capacity` minus
`NODELET_SYSTEM_RESERVED_*`/`NODELET_KUBE_RESERVED_*`, both `0` by
default) — **the cgroup v2 writes are unvalidated against a real
`/sys/fs/cgroup`**, best-effort/logged-not-fatal on failure, see
`docs/GAP_CLOSURE.md`'s round 11 notes. `RuntimeClass.overhead` is wired
through too, closed alongside this round's cgroup work. Also
**PersistentVolumeClaim/CSI** (`runtime/csi.rs`, `plugin_registry.rs`) —
a bound PVC's CSI-backed `PersistentVolume` gets staged/published via a
CSI driver's Node service and bind-mounted into the container like any
other volume. Driver discovery is both static (`NODELET_CSI_DRIVERS`) and
dynamic — a driver's `node-driver-registrar` sidecar can register itself
against `NODELET_PLUGIN_REGISTRY_PATH`, the same protocol it'd use against
real kubelet's own plugin watcher — and `nodeStageSecretRef`/
`nodePublishSecretRef` are resolved and passed through. **Attach
coordination** (round 19) — checks `CSIDriver.spec.attachRequired`, and
for drivers that need it, waits on the matching
`VolumeAttachment.status.attached` before Stage/Publish, passing
`status.attachmentMetadata` through as `publish_context`; calling the
Controller service itself stays out of scope, confirmed against docs that
`ControllerPublishVolume`/`ControllerUnpublishVolume` are
external-attacher's job upstream, not kubelet's —
**unvalidated against a real CSI driver**, see `docs/GAP_CLOSURE.md`'s
round 12/13/19 notes. Also **device plugins** (`device_plugins.rs`,
GPU/FPGA/etc. hardware) — discovered via the same dynamic registration
protocol as CSI drivers, advertised on `Node.status.capacity`/
`.allocatable`, and allocated into requesting containers'
envs/mounts/device-nodes via the plugin's `Allocate()` RPC —
**unvalidated against real hardware**, see round 14 notes. Also **CPU
Manager** (`cpu_manager.rs`, `NODELET_CPU_MANAGER_POLICY=static`,
disabled by default) — Guaranteed-QoS containers requesting a whole
number of CPUs get pinned to exclusive cores, and (round 16)
already-running shared-pool containers get retroactively shrunk/grown via
CRI's `UpdateContainerResources` as exclusive claims are made/released,
matching real kubelet's bidirectional behavior. Also **Memory Manager**
(`memory_manager.rs`, `NODELET_MEMORY_MANAGER_POLICY=static`, disabled by
default) — Guaranteed-QoS containers with a memory limit get pinned to a
single NUMA node (never spans multiple nodes, no shared-pool tracking for
non-pinned containers — see round 18 notes), and **Topology Manager**
(`topology.rs`, `NODELET_TOPOLOGY_MANAGER_POLICY`, disabled by default) —
coordinates CPU Manager, Memory Manager, and device plugins so a
container's exclusive cores, pinned memory, and allocated devices all
land on the same NUMA node; `restricted` policy (round 20) gets a real,
bounded multi-node relaxation via `topology::spread()` — each provider
placed on its own best node independently when no single node satisfies
everyone — while `single-numa-node` stays strict single-node-only, and
neither searches upstream's full joint cross-provider bitmask/permutation
space (see round 17/18/20 notes), reads real NUMA topology from
`/sys/devices/system/node`. Device plugins also gained (round 21)
`GetPreferredAllocation` (a plugin's own device choice, validated before
trust, falling back to nodelet's own pick otherwise) and
`PreStartContainer` (called right after `Allocate()` succeeds, for
plugins that require it). A fresh gap re-audit against kubelet's own
docs (round 22) found and closed **pod `readinessGates`** (round 23) —
`spec.readinessGates` lets an external controller (a service mesh
sidecar, say) hold a pod's `Ready` condition `False` until its own named
condition is `True`, alongside the built-in `ContainersReady`; closing
this also fixed a real pre-existing bug where nodelet's JSON-Merge-Patch
status writes silently deleted any condition an external controller had
set (the whole `conditions` array got replaced wholesale) — now foreign
conditions are carried forward on every write. Also closed (round 24):
**`terminationMessagePath`/`terminationMessagePolicy`** — a container's
termination-log file is now bind-mounted and read back into
`ContainerStatus.state.terminated.message`, which surfaced a bigger
pre-existing gap along the way — regular/init containers never reported
a real `terminated` state at all before this round, always
`Waiting: ContainerCreating` forever once exited. `FallbackToLogsOnError`
is a documented, deliberate simplification not implemented. Also closed
(round 25): **user namespaces** (`spec.hostUsers: false`, `src/userns.rs`)
— each such pod gets an exclusive host UID/GID range via CRI's
`userns_options`, a fixed-length allocator (not upstream's variable-length
pool) with in-memory-only state. Also closed (round 26): **eviction
priority-tiebreaking** — `pick_eviction_candidate()` now ranks by
`spec.priority` (already resolved by the apiserver's own Priority
admission controller, no `PriorityClass` lookup needed) before falling
back to usage, matching real kubelet's own ordering; this closes every
candidate round 22's fresh gap re-audit found. A second re-audit (round
27) found more gaps, and round 28 closed the highest-value one:
**`oom_score_adj`** — `linux_resources()` now sets CRI's per-container
`oom_score_adj` from real kubelet's own formula (Guaranteed `-998`,
BestEffort `1000`, Burstable scaled by that container's own memory
request against node capacity), giving the kernel OOM killer QoS-aware
signal — closing a real gap in this project's own eviction-manager story
(rounds 7, 26), since a kernel OOM kill can happen faster than
`eviction_loop()`'s own check interval reacts. Also closed (round 29):
**gRPC probes** — `probe.grpc` now dials the standard
`grpc.health.v1.Health/Check` protocol via a vendored client
(`proto/health.proto`, `cri`-gated); failure paths (timeout, refused,
non-gRPC listener) are unit-tested, the success path is unvalidated (no
gRPC server available to test against live). Also closed (round 30):
**`emptyDir.medium: Memory`** — `resolve_volumes()` now mounts real
tmpfs (`mount -t tmpfs`) for it, honoring `sizeLimit`, and `remove_pod()`
unmounts it again on teardown (a real RAM leak otherwise). Also closed
(round 31): **generic ephemeral volumes** (`spec.volumes[].ephemeral`) —
resolves the ephemeral-volume controller's deterministic-named
(`<pod name>-<volume name>`) PVC, with an ownership safety check by UID,
then reuses all of CSI's existing mount machinery. Also closed (round
32): **image volume source** (`volumeSource.image`) — uses CRI's native
`Mount.image`/`image_sub_path` fields directly after a `PullImage` call,
no host-path materialization needed (unlike every other volume kind);
always read-only, per the KEP. Also closed (round 33):
**`Node.status.images`** — reports CRI's cached images, largest-first,
capped at 50 (matching real kubelet's own default). Also closed (round
34, the last round-27 candidate): **`Node.status.volumesInUse`/
`.volumesAttached`** — scoped to CSI volumes only, reusing the mount
reference-counting round 12 already tracked; deliberately
lower-confidence by design since whether a real attach/detach controller
is satisfied by this is unvalidated (the modern CSI attach path, round
19, doesn't read these fields itself). A fresh gap re-audit (round 35)
found more gaps, and round 36 closed the highest-value one: **native
sidecar containers** (`initContainers[].restartPolicy: "Always"`, GA
since 1.29) — a sidecar-marked init container no longer blocks later
init/app containers on its own exit (only on having started), restarts
indefinitely like a normal container, and its real probe-based readiness
folds into the pod's overall `Ready` condition. Teardown ordering
(sidecars stopped strictly last) is a documented simplification, not yet
matching upstream exactly. Round 37 closed **ConfigMap/Secret
live-update** — the pod controller now watches referenced ConfigMap/
Secret objects (cluster-wide; they have no node-scoping fieldSelector)
and re-materializes any affected pod's volumes within seconds of a
change, overwriting the already-live bind-mounted host files with no
pod/container restart needed — the well-known "edit a ConfigMap, the
mounted file updates live" behavior. Deliberately scoped to
volume-mounted references only, not env vars (`envFrom`/`valueFrom...Ref`),
matching real kubelet exactly. Round 38 closed **`spec.hostname`/
`subdomain`/`setHostnameAsFQDN`** — the CRI sandbox's hostname now
honors an explicit `spec.hostname` override (default the pod name), and
`setHostnameAsFQDN` (only meaningful with `spec.subdomain` also set)
makes the sandbox's actual hostname the full
`<hostname>.<subdomain>.<namespace>.svc.<cluster-domain>` FQDN instead
of just the short name — rejecting, not silently truncating, an FQDN
over Linux's 64-byte hostname limit, matching real kubelet's own hard
failure there. A fresh gap re-audit (round 39) found 4 more gaps, the
highest-value being **in-place pod vertical scaling** (the `resize`
subresource, GA in 1.33) — editing a running pod's CPU/memory request
or limit today does nothing at all, not even a container restart;
`hostPID`/`hostIPC`/`shareProcessNamespace` and `securityContext.sysctls`
are also unset. Round 40 closed **`hostPID`/`hostIPC`/
`shareProcessNamespace`** — and fixed a real correctness bug along the
way: nodelet never set CRI's `NamespaceOption.pid` at all before this,
so every container was silently getting containerd's own POD-shared PID
default, the opposite of real Kubernetes' actual CONTAINER-scoped
default. Every container now gets an explicit, correct PID-namespace
mode (`hostPID` → shares the node's; `shareProcessNamespace` → shares
one namespace across the pod; otherwise its own isolated one, matching
upstream). Round 41 closed **`securityContext.sysctls`** — flattened
into CRI's `LinuxPodSandboxConfig.sysctls` map, the same field
`sandbox_config()` already populates for `cgroup_parent`/`overhead`.
Round 42 started the **in-place pod vertical scaling** arc (the
`resize` subresource, GA in 1.33): editing a running pod's CPU/memory
now actually does something — applied live via CRI's
`UpdateContainerResources` when `resizePolicy` allows it, or a real
container restart when it doesn't — instead of the previous no-op.
Round 43 finished the arc: `containerStatuses[].resources`/
`.allocatedResources` (app containers) and a `PodResizeInProgress`
condition now report what's actually happening — `PodResizePending`
stays unimplemented on purpose, since nodelet has no admission/
node-fitting layer that could ever *defer* a resize. Round 44 closed
out the last two known audit findings: env `valueFrom.resourceFieldRef`
(reproducing kubelet's "CPU reports whole cores, rounded up" quirk and
the common JVM-heap-sizing memory-divisor pattern) and a liveness
probe's own `terminationGracePeriodSeconds` override (previously a
hardcoded 10s regardless of pod or probe settings). A fresh gap
re-audit (round 45) confirmed several plausible candidates were already
implemented, and found 2 new gaps plus generalized one: **CSI
ephemeral (inline) volumes** (`volumes[].csi` directly, not PVC-based —
likely cheap given the CSI Node-service plumbing already exists),
**startup probe failure never triggers a restart** (retries forever
instead of killing/restarting past `failureThreshold`), and **local
ephemeral storage isn't tracked anywhere** (capacity, requests/limits,
or eviction). Round 46 closed **CSI ephemeral (inline) volumes**
(`volumes[].csi` specified directly, no PVC at all — the form drivers
like `secrets-store-csi-driver` use) by reusing all of the existing
CSI Node-service plumbing (rounds 12/13/19) as-is; `CsiDrivers::mount()`/
`unmount()` now correctly skip staging/attach entirely for this volume
kind, per the CSI spec's own rule rather than a driver-capability
check. Round 47 closed **startup probe failure restart** — a startup
probe failing past its own `failureThreshold` now kills and restarts
the container, matching real kubelet's liveness-probe-like behavior,
instead of retrying forever with no restart at all. Round 48 started
the **local ephemeral storage** arc: `Node.status.capacity`/
`.allocatable["ephemeral-storage"]` now reports the real filesystem
size backing the node's disk path, reusing the same `statvfs(2)` read
`DiskPressure` already makes. Round 49 finished it: a pod exceeding its
own `ephemeral-storage` limit (measured from CRI's per-container
writable-layer usage plus a walk of nodelet's own materialized volume
directory) is now evicted directly, independent of general node
pressure — the same relationship an individual container's OOM kill
has to overall node memory. A fresh gap re-audit (round 50) found 3
more gaps, the highest-value being **`imagePullPolicy` is completely
unenforced** — every container's image gets pulled unconditionally
regardless of `Always`/`IfNotPresent`/`Never`, which cuts against this
project's own edge/offline-capable design goal; `ContainerStatus.imageID`
is also always empty (CRI already returns the digest, just unread),
and `Node.status.runtimeHandlers` is never reported (the CRI `Status`
RPC is never called). Round 51 closed the first: `imagePullPolicy`
(`Always`/`IfNotPresent`/`Never`) is now actually enforced, including
real kubelet's own default-policy heuristic when unset — `Never`
refuses to pull at all instead of silently doing so, and
`IfNotPresent` skips the registry round-trip entirely once an image
is cached, both real wins for genuinely offline edge operation. Round 52 closed
**`ContainerStatus.imageID`** — CRI's own `Container.image_ref` (a
digested image reference, already fetched every reconcile) is now
carried through instead of always reporting the empty string. Round 53
closed the last item: **`Node.status.runtimeHandlers`** now reports
the discovered RuntimeClass handlers via CRI's runtime-level `Status`
RPC (never called before this round) — all 4 audit lists to date were
fully closed as of that round. A fresh gap re-audit (round 54) found 3
more gaps: **`PodStatus.qosClass` is never set** (nodelet already
computes this internally for eviction ranking, just never surfaces it
— likely the cheapest fix), `PodStatus.hostIPs` (plural, dual-stack)
is never set, and `ContainerStatus.containerID` is missing its
`<runtime>://` scheme prefix real kubelet always includes. Round 55
closed the first: **`PodStatus.qosClass`** is now reported, reusing
the `eviction::qos_class()` computation nodelet already had internally
for eviction ranking since round 7. Round 56 closed the next: the
plural **`PodStatus.hostIPs`** is now set alongside the existing
singular `hostIP`, mirroring the already-correct `podIP`/`podIPs`
split. Round 57 closed the last item: `ContainerStatus.containerID`
now gets the real `<runtimeName>://<id>` scheme prefix (e.g.
`containerd://...`), from a new one-time CRI `Version` call — all 5
audit lists to date were fully closed as of that round. A fresh gap
re-audit (round 58) found 3 more gaps: **HugePages support is entirely
missing** (container resource limits, `Node.status.capacity`
reporting, and the `emptyDir.medium: HugePages` volume form),
`securityContext.supplementalGroupsPolicy` (GA 1.33) is never read,
and Dynamic Resource Allocation (`spec.resourceClaims`) is
unimplemented — flagged for completeness, though its value-to-
complexity ratio for a single-node edge kubelet is genuinely
questionable. Round 59 closed the cheapest HugePages piece: container
`resources.limits["hugepages-<size>"]` is now translated to CRI's
`LinuxContainerResources.hugepage_limits` (a field the vendored proto
already had, unused until now) via new `hugepage_limits()`/
`hugepage_cri_page_size()` helpers — the latter a naming-convention
translation only (k8s's `Mi`/`Gi`/`Ki` suffix to CRI's `MB`/`GB`/`KB`
page_size string; the byte value itself is unchanged). Round 60 closed
the next HugePages piece: **`Node.status.capacity`/`.allocatable
["hugepages-<size>"]`** now report every hugepage pool actually reserved
on the node, read straight from `/sys/kernel/mm/hugepages/` (no CRI RPC
involved) — unreserved pool sizes are omitted entirely rather than
reported as zero, matching real kubelet. Round 61 closed the last
HugePages piece: **`emptyDir.medium: "HugePages"`/`"HugePages-<size>"`**
volumes are now real `hugetlbfs` mounts (via `mount(8)`, the same
host-tool approach round 30's tmpfs support already established),
closing round 58's HugePages audit item entirely. Round 62 closed the
`supplementalGroupsPolicy` item: **`securityContext.supplementalGroupsPolicy`**
(`Merge`/`Strict`) now translates directly to CRI's own
`SupplementalGroupsPolicy` enum, which had direct native support already.
Round 63 implemented **Dynamic Resource Allocation** (`spec.resourceClaims`):
kubelet's actual DRA responsibilities — resolving a pod's claims to their
`ResourceClaim.status.allocation`, calling the owning driver(s)'
`NodePrepareResources`/`NodeUnprepareResources` over a new gRPC plugin
protocol (reusing the same registration infrastructure CSI drivers and
device plugins already use), and wiring the returned CDI device IDs into
each container's CRI config. Round 64 closed round 63's 2 known scope
limitations: `NodePrepareResources`/`NodeUnprepareResources` are now
batched (one call per driver per pod, covering every claim it owns,
instead of one call per claim), and preparation is now gated on
`ResourceClaim.status.reservedFor` actually listing the pod — the real
safety check kubelet performs, correcting round 63's docs which
mistakenly said kubelet writes that field (it's scheduler-written,
kubelet-read). No genuinely automated e2e test exists yet — it needs a
real DRA driver binary this project's bash-only test harness can't stand
up. A fresh gap re-audit (round 65) found **hostPath volumes** were still
explicitly unsupported (silently dropped) since early on — closed:
`spec.volumes[].hostPath` now mounts the host's own real path directly,
with full `type` validation (`DirectoryOrCreate`/`FileOrCreate`/
`Directory`/`File`/`Socket`/`CharDevice`/`BlockDevice`) matching real
kubelet's own create-vs-require-existing semantics. Round 66 closed
`lifecycle.stopSignal` (GA 1.33), also found in that audit: translates
directly to CRI's own `Signal` enum (native support, never wired up
before), with a genuinely automated e2e test proving a non-default
signal (`SIGUSR1`) actually gets delivered to the container. Round 67
closed `emptyDir.sizeLimit` enforcement (the last audit item that wasn't
swap support): a plain-disk `emptyDir` volume exceeding its own
`sizeLimit` now evicts the pod, checked independently of both the
whole-pod ephemeral-storage limit (round 49) and general node-pressure
eviction — scoped to plain-disk `emptyDir` only, since a `Memory`/
`HugePages`-medium volume's `sizeLimit` is already a real kernel-enforced
cap at mount time. Round 68 closed the last of round 65's audit
candidates, swap support (`memorySwap.swapBehavior`, GA 1.34): a new
`NODELET_MEMORY_SWAP_BEHAVIOR` knob (`NoSwap` default / `LimitedSwap`)
drives CRI's native `memory_swap_limit_in_bytes`, implementing upstream's
KEP-2400 proportional-share formula exactly for `LimitedSwap` — with a
genuinely automated e2e test proving the default `NoSwap` behavior's real
cgroup effect. A fresh gap re-audit (round 69) found `volumeMounts[].subPathExpr`
still unimplemented — closed: `$(VAR)` references now expand against a
container's own resolved env (most commonly Downward API values like
`$(POD_NAME)`), with an unresolvable reference dropping the mount rather
than substituting a garbage path. Round 70 closed **image GC watermark
policy**: real kubelet's disk-pressure-triggered high/low threshold
policy (`NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT`/`_LOW_THRESHOLD_PERCENT`/
`_MIN_AGE_SECS`, matching upstream's own flags) replaces the previous
unconditional every-cycle unreferenced-image sweep — an unreferenced
image is now left alone until disk usage actually crosses the high
threshold, then removed oldest-unreferenced-first. Round 71 closed
**image credential providers** (`--image-credential-provider-config`/
`-bin-dir`, ServiceAccount token integration beta/default-on in k8s
1.34): a `CredentialProviderConfig` YAML lists exec-plugin binaries and
`matchImages` glob patterns; on a pull that no `imagePullSecret` resolves,
nodelet execs the first matching provider, optionally minting it an
audience-scoped `ServiceAccount` token (reusing the same `TokenRequest`
machinery projected `serviceAccountToken` volumes already use) when the
provider declares `tokenAttributes`. A fresh gap re-audit (round 72)
found crash-loop backoff missing entirely — closed round 73: a container
that keeps exiting is now throttled with an exponentially growing delay
(10s base, doubling, capped at 5 minutes, matching real kubelet's own
constants) instead of being recreated as fast as this event-driven
controller's own status-write-triggers-another-watch-event feedback loop
otherwise allows. Round 74 closed the **PodResources API**: kubelet's own
`List`/`GetAllocatableResources`/`Get` gRPC service, served over a Unix
socket (`NODELET_POD_RESOURCES_SOCKET_PATH`) for external device-monitoring
tooling (NVIDIA DCGM and similar exporters) — a read-only projection of
CPU/Memory/device-manager state this codebase already tracked, not new
allocation logic; DRA claim devices aren't surfaced yet (documented scope
limitation). Round 75 closed the display gap round 73 deliberately left
open: `containerStatuses[].lastState` is now tracked, so a backing-off
container's current state reports `Waiting{reason: CrashLoopBackOff}`
(its real exit details moved into `lastState` instead) rather than
`Terminated` — matching kubectl's familiar display. A fresh gap re-audit
(round 76) found raw block volumes never wired up — closed round 77:
`spec.containers[].volumeDevices` + a PV's `volumeMode: Block` now inject
the raw device via CRI's `ContainerConfig.devices` (the same mechanism
device-plugin device-node injection already uses), with CSI's own
`AccessType::Block` and a file-shaped bind-mount target instead of the
usual directory. Round 78 closed `securityContext.procMount`: nodelet
previously never masked `/proc` for any container at all (a modern
containerd applies zero masking when `masked_paths`/`readonly_paths` are
left unset) — it now always sends them explicitly, matching real
kubelet's own posture, with `Default` getting the standard masked/readonly
lists and `Unmasked` getting genuinely empty ones. Round 79 closed
`allocatedResourcesStatus` (KEP-4680): `containerStatuses[]` now reports
live per-device health for device-plugin allocations, closing the last
item from round 72's fresh gap re-audit. A fresh gap re-audit (round 80)
found `spec.activeDeadlineSeconds` never implemented — closed round 81:
a pod running past its own deadline is now terminated regardless of
`restartPolicy`, checked as a direct per-pod violation alongside the
existing ephemeral-storage/emptyDir eviction tiers. Round 82 closed
`spec.containers[].ports[].hostPort`: CRI's own `PortMapping` field
(vendored but never wired up, same shape as round 77's raw block volumes
finding) now publishes an explicit `hostPort` on the node's own IP,
empty for `hostNetwork` pods matching upstream. Round 84 closed
`volumeMounts[].mountPropagation`: `HostToContainer`/`Bidirectional` now
translate to CRI's own `MountPropagation` enum on every mount, unset
mounts unaffected (same `PRIVATE` default as before). Round 85 closed
`volumeMounts[].recursiveReadOnly` (GA 1.33): the API's ternary now
translates to CRI's `Mount.recursive_read_only`, defensively enforcing
the proto's own `readonly: true` + `Private`-propagation contract. Round
87 closed `PodCondition.observedGeneration`: every condition nodelet
writes now carries the pod's own `metadata.generation`, unchanged unless
the condition's status actually flips — matching real kubelet's own
semantics exactly. Round 88 closed per-volume `Mount.uidMappings`/
`.gidMappings`: every volume mount for a `hostUsers: false` pod now
carries the same UID/GID range `run_sandbox()` already applies at the
sandbox level, so kernel-level idmapped-mounts translation actually
applies to volumes too, not just the sandbox itself. Round 90 closed
`containerStatuses[].user`: the resolved UID/GID/`supplementalGroups` a
container's first process actually started with is fetched once, right
after the container starts, and cached — no extra RPCs on a healthy
container's ongoing reconciles. Round 91 closed
`containerStatuses[].volumeMounts[].recursiveReadOnly`: what an
`IfPossible`/`Enabled` mount actually resolved to, computed once at
container-creation time straight from the spec (no RPC at all, matching
real kubelet's own approach — CRI has no volume-name concept to read
this back from). Round 93 closed `fsGroupChangePolicy` for CSI-mounted
volumes (matching upstream, which only ever honors this field for
PV-backed volume types) and fixed a related latent gap along the way:
`fsGroup` is no longer applied to real `hostPath` volumes at all,
matching upstream's own no-ownership-management stance for that volume
type. Round 94 added `NODELET_CONFIG_FILE`/`NODELET_CONFIG_DIR` — a
YAML file (or drop-in directory) mapping the same `NODELET_*` keys the
environment already reads, as an alternative to env-var-only
configuration. Round 95 added optional TLS client certificate
authentication (`NODELET_CLIENT_CA_FILE`): a request with a cert
chaining to the configured CA authenticates directly off its Subject
CN/O (no `TokenReview` round-trip), matching real kubelet's own
x509-then-bearer-token authenticator chain, while a request with no
cert still falls back to the existing bearer-token path unchanged.
Round 96 added TLS bootstrap (`NODELET_BOOTSTRAP_KUBECONFIG`): given a
low-privilege bootstrap credential, nodelet generates a keypair,
submits a `certificates.k8s.io/v1` CertificateSigningRequest for its
own node identity, and — once the apiserver's own node-authorizer
approves and signs it — writes a real client-cert kubeconfig for all
further apiserver traffic, the same `--bootstrap-kubeconfig` flow real
kubelet uses instead of always needing a working kubeconfig handed to
it directly. Round 97 gave `volumeMounts[].recursiveReadOnly: IfPossible`
a real best-effort fallback: it now resolves to `true` only when the
pod's resolved runtime handler actually advertises
`recursiveReadOnlyMounts` support, falling back to a plain read-only
mount otherwise, instead of always behaving like `Enabled`. Full
status list in `docs/GAP_CLOSURE.md`.

**Scope note:** nodelet keeps its single-node-first design (this is where
it shines — low idle CPU, no etcd/multi-node coordination overhead for
the common edge case), but the project itself is no longer scoped to
single-node deployments only: it's meant to be a genuine drop-in kubelet
replacement usable in ordinary multi-node clusters too. That reopens
scope for multi-node-relevant gaps previously deferred as "not worth it
for a single-node edge kubelet" — most notably Dynamic Resource
Allocation (`spec.resourceClaims`), now a real target rather than a
flagged-for-completeness item.

---

## Build

Requires a Rust toolchain (stable, and new enough — check `Cargo.lock` for
the pinned `kube`/`tonic` versions' actual MSRV if `cargo build` fails with
"rustc X is not supported by the following packages"; distro-packaged Rust
is frequently too old). The default build needs **no** extra system
packages; the `cri` feature needs `protoc` at build time.

```bash
# Default build — mock runtime only, no protoc needed:
cargo build --release
# -> target/release/nodelet

# With the real containerd/CRI runtime:
sudo apt-get install -y protobuf-compiler   # provides protoc
cargo build --release --features cri
```

### Verify it builds

```bash
cargo build --release                 # mock
cargo build --release --features cri  # cri
cargo test                            # (unit tests, if any)
```

Both invocations should end in `Finished ... target(s)`.

---

## Try it in one command

`deploy/bootstrap-source.sh` is a single, self-contained script that installs
and smoke-tests the *entire* stack on any Linux box, regardless of distro or
CPU architecture — one command, nothing to install by hand first. It
re-execs itself under `sudo` automatically (root is needed to install system
packages and the k3s service), detects your package manager and arch, gets a
C toolchain and a Rust toolchain new enough to build this workspace (falling
back through official prebuilt releases, then a static cross toolchain, then
building gcc from source if truly nothing else is available), builds
`nodelet`, installs and starts the stripped k3s control plane, and applies
the demo pod. With `--with-cri` it also installs containerd + runc (package
manager -> official prebuilt -> built from source via a from-scratch Go
toolchain bootstrap) and starts containerd itself.

`nodelet` itself is installed as a real, persistent service — the same
treatment k3s already gets — not just started in the foreground and left to
die with the terminal: a systemd service (`Restart=always`, enabled on boot)
if systemd is present, an OpenRC service (`supervise-daemon`, added to the
default runlevel) if not — k3s's own installer only supports those two init
systems, so nothing actually running k3s should lack both. If neither is
present, a self-restarting background loop plus a cron `@reboot` entry
(best-effort — not a substitute for a real service, and it says so when it
falls back to this).

**Minimal footprint by default:** the point of this project is not wearing
out an embedded device's flash or leaving a permanent toolchain installed on
it. Once the build (and any from-source containerd/runc/CNI/flannel builds)
finishes, the script copies the binary to `bin/nodelet`, deletes `target/`
(the whole cargo build cache), deletes every download/git-clone/source
directory it used, and uninstalls every build-only package (`rustc`/`cargo`,
the C/C++ toolchain, `protoc`, `go`, `git` if it wasn't already there) —
**only** ones this run installed fresh, never something that pre-existed,
and never runtime pieces the cluster keeps needing (containerd, runc,
flanneld, CNI plugins, nftables, k3s). Pass `--keep-build-tools` to skip
this, e.g. while iterating on the script itself. If a run fails partway —
a version gate going stale again, a network blip mid-download — this same
cleanup runs automatically on the way out, so a failed attempt doesn't
leave a half-installed toolchain behind with no way back.

```bash
./deploy/bootstrap-source.sh                     # installs everything, mock runtime
./deploy/bootstrap-source.sh --with-cri          # + containerd/runc, real containers
./deploy/bootstrap-source.sh --with-cri --ip-family=ipv4      # force v4-only (default: auto)
./deploy/bootstrap-source.sh --with-cri --lb-method=round-robin  # default: random
./deploy/bootstrap-source.sh --skip-control-plane  # bring your own KUBECONFIG, no root needed
./deploy/bootstrap-source.sh --keep-build-tools  # skip the end-of-run toolchain cleanup
./deploy/bootstrap-source.sh --cleanup           # stop the deployment, keep runtime pkgs/k3s for next time
./deploy/bootstrap-source.sh --uninstall         # full teardown: also k3s, containerd/runc, CNI/flannel, nftables
./deploy/bootstrap-source.sh --uninstall --force # same, but by name — for a machine an older/untracked run left dirty
```

`--cleanup` vs `--uninstall`: `--cleanup` stops what a run started (nodelet,
flanneld, containerd, the nft Service table) and removes this script's own
scratch — enough to start clean, but leaves runtime packages and k3s
installed so the next run is fast. `--uninstall` is the full teardown: k3s's
own data/config, containerd/runc's state and binaries, and all CNI/flannel
config/binaries too — but, same rule as everywhere else in this script, only
for what it actually installed. If containerd/runc or the CNI/flannel setup
predate this script (e.g. Docker's containerd was already there), they and
their state are left completely untouched — this is checked the same way
package installs are (nothing recorded as installed by this run means
nothing gets removed), plus a check for the case where a stray process from
an earlier, unrelated run of this script is still alive with no matching
pidfile.

That ownership tracking only exists for runs of the current version of this
script — it can't know what an *older* version installed, since the tracking
(`pkg_installs.log`) didn't always exist. If a machine was left dirty by a
run from before this flag existed, plain `--uninstall` will correctly find
nothing it recognizes and do nothing useful. `--uninstall --force` is for
exactly that: it skips every ownership check and removes k3s, containerd/
runc, CNI plugins, flannel, and nftables **by name**, whether or not this
exact script installed them. Verified for real: installed cargo/rustc/
containerd/runc/nftables/git with no tracking log present at all (simulating
that "old version, dirty machine" case) — plain `--uninstall` correctly left
all of it in place, `--uninstall --force` removed all of it. This is real
fallout, not just a stronger default — it can remove packages/config you set
up yourself outside this project if they happen to share these names (in
testing this, it also removed the sandbox's own `git`). Use it when you know
the machine's state is this project's mess to clean up, not a shared box.

Rust itself has no from-source bootstrap path (every rustc needs an existing
rustc to build it); the script uses rustup's official prebuilt toolchains,
which cover the realistic edge/embedded architecture set (x86_64, aarch64,
armv7/armv6, i686, riscv64gc, ppc64le, s390x, loongarch64). k3s's own release
binaries only cover amd64/arm64/armhf/s390x — on anything else the script
skips control-plane setup and tells you so rather than guessing.

## Pod networking (CNI)

Host-network-only defeats a lot of the point of using Kubernetes, so
`--with-cri` wires up real CNI networking by default (`--cni=flannel`;
`--cni=none` restores the old hostNetwork-only behavior).

How it fits together, with no changes needed to k3s or nodelet's core loop:
- `nodelet` already only forces `hostNetwork` when a Pod asks for it
  (`crates/nodelet/src/runtime/cri.rs`) — any other pod is left for containerd
  to run through its normal CRI-driven CNI path.
- containerd's CRI plugin is the thing that actually invokes CNI plugins on
  `RunPodSandbox`; it just needs plugin binaries in `/opt/cni/bin` and a
  network config in `/etc/cni/net.d`, which `bootstrap-source.sh` installs.
- Per-node pod subnet allocation needs no new code on the control-plane side:
  k3s's controller-manager runs `--allocate-node-cidrs` regardless of
  `--flannel-backend`, which is exactly what lets `--flannel-backend=none`
  (already the flag `setup-control-plane.sh` uses) be paired with any
  self-managed CNI — flannel included.
- `bootstrap-source.sh` installs the standard CNI plugins
  (bridge/host-local/portmap/...), flannel's daemon (`flanneld`) and its CNI
  plugin, writes `/etc/cni/net.d/10-flannel.conflist`, and runs `flanneld
  --kube-subnet-mgr` — flannel's own IPAM backend that reads/writes per-node
  subnet leases straight from Node objects, no separate etcd needed.

Flannel was picked because it's the lightest widely-deployed option (one
small daemon, no extra datastore) — a reasonable "start here" for a
resource-constrained node. `--cni` is a real dispatch point, not just a
flannel flag: adding another plugin (Calico, Cilium, ...) means installing
its binaries/config and starting its daemon in `ensure_cni()` — containerd
and nodelet don't care which CNI is in use.

### IPv4 / IPv6 / dual-stack

`--ip-family` controls which address families k3s, flannel, and nodelet's
Service proxy all use — the same value is handed to all three so they can't
disagree:

| Value | Behavior |
|---|---|
| `auto` (default) | Detect what the node actually has: both stacks work → `dual`; only one does → that one. Detection is a real socket bind in each family (`0.0.0.0:0` / `[::]:0`), not `/proc` parsing. |
| `ipv4` | v4 only — the original, most battle-tested path. |
| `ipv6` | v6 only. |
| `dual` | Both. `--cluster-cidr`/`--service-cidr` become comma-separated v4,v6 pairs (`10.42.0.0/16,fd00:42::/48` and `10.43.0.0/16,fd00:43::/112` — the same values Rancher's own k3s dual-stack docs use), and flannel gets a `net-conf.json` with `EnableIPv6`. |

The nftables side (`crates/nodelet/src/svc.rs`) uses a single `inet`-family
table that mixes `ip`/`ip6` matches directly, and resolves backends from
`EndpointSlice` (not the legacy `Endpoints` API) specifically because
dual-stack Services get separate slices per address family — the legacy API
only ever mirrors one family, which would silently break v6 backends.

**Honesty note:** the nftables rule generation for all three IP-family modes
is verified against a real `nft -c` parse for every load-balancing method ×
backend count combination (see `cargo test`, which requires root/CAP_NET_ADMIN
to actually run the check). What isn't independently verified is the
IPv6/dual-stack **vxlan dataplane** flannel sets up — that needs real
dual-stack network hardware/CAP_NET_ADMIN this project wasn't built against.
Treat `--ip-family=ipv6`/`dual` as less battle-tested than the IPv4 path.

## Services (ClusterIP / NodePort), no kube-proxy

Getting pods real IPs is only half the story — Services route through a
virtual ClusterIP that no interface ever owns, which is normally kube-proxy's
job. Rather than run kube-proxy (part of the kubelet/agent this project
deliberately doesn't run), `nodelet` does that job itself:
`crates/nodelet/src/svc.rs` watches Services + EndpointSlices the same
event-driven way `pods.rs` watches Pods, and programs an nftables table
(`not_k8s_svc`) with the current ClusterIP/NodePort → backend mappings. No
separate process, no periodic resync — the whole ruleset is rebuilt
atomically only when a Service or EndpointSlice actually changes.

**Load balancing** — the common algorithms that are actually feasible with
stateless nftables (no per-connection counters like ipvs's least-conn):

| Method | nft mechanism | When |
|---|---|---|
| `random` (default) | `numgen random mod N` | `NODELET_LB_METHOD=random`, or unset |
| `round-robin` | `numgen inc mod N` (deterministic counter) | `NODELET_LB_METHOD=round-robin` |
| `source-hash` | `jhash ip[6] saddr mod N` (sticky per client IP) | `NODELET_LB_METHOD=source-hash`, **or automatically** whenever a Service sets the real Kubernetes field `sessionAffinity: ClientIP`, regardless of the configured default |

This needs `nft` on the node and bridged pod traffic to reach the host's
netfilter tables (`br_netfilter`) — both handled by
`deploy/bootstrap-source.sh --with-cri`. If `nft` isn't available, `nodelet`
detects that, logs it once, and simply skips Service routing; direct pod-IP
traffic is unaffected either way. `NODELET_SERVICE_PROXY=false` opts out
explicitly (it's on by default whenever `NODELET_RUNTIME=cri`).

## Run it (single device, offline)

### 1. Bring up the stripped control plane (no kubelet)

```bash
sudo ./deploy/setup-control-plane.sh
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
```

This installs k3s with `--disable-agent` (the key flag: it removes the built-in
kubelet so `nodelet` can take its place) plus a pile of `--disable` flags to strip
everything an edge node doesn't need. It's a *real* apiserver, so `kubectl` and CRDs
work 1:1.

### 2. Start the node agent

```bash
# mock runtime (no container engine needed — great for measuring the control loop):
KUBECONFIG=/etc/rancher/k3s/k3s.yaml NODELET_RUNTIME=mock ./deploy/run-nodelet.sh

# or with real containers (requires containerd + a built `--features cri` binary):
KUBECONFIG=/etc/rancher/k3s/k3s.yaml NODELET_RUNTIME=cri \
  NODELET_CRI_ENDPOINT=unix:///run/containerd/containerd.sock ./deploy/run-nodelet.sh
```

In another shell:

```bash
kubectl get nodes            # the nodelet node should be Ready
kubectl apply -f deploy/demo-pod.yaml
kubectl get pods -w          # goes Pending -> Running (mock fakes the container)
kubectl apply -f deploy/demo-nginx.yaml
kubectl get deploy,pods
```

With `mock`, pods report `Running` without real containers — this proves the entire
control loop (schedule → bind → node agent → status) end-to-end with ~zero overhead.
With `cri`, they run for real on containerd.

### 3. Measure the overhead

```bash
./deploy/measure.sh
```

Samples idle CPU% and RSS of the k3s control plane and `nodelet` so you can compare
against stock k3s (~30–50% of a core, ~15% RAM). The win should show up on the node
side — `measure.sh` only tells you *that* k3s-server itself is still using CPU/RAM,
not *why*.

### 4. Diagnose k3s-server itself

`nodelet` and `bootstrap-source.sh` only ever touched the node-agent side (the
kubelet replacement) — k3s-server (apiserver + scheduler + controller-manager
+ kine, its embedded sqlite datastore, all bundled into one process) has
never been profiled or trimmed. If it's still using significant idle CPU/RAM,
this collects the evidence needed to find out what, instead of guessing:

```bash
sudo ./deploy/diagnose-control-plane.sh        # 30s sample by default
sudo ./deploy/diagnose-control-plane.sh 60     # or pick your own sample length
```

Pulls real Go pprof CPU/heap profiles from each embedded component (via
`kubectl get --raw /debug/pprof/...` for the apiserver, and client-cert auth
against the controller-manager/scheduler secure ports), a goroutine dump, an
`strace -c` syscall summary (this is what catches kine's SQLite polling — a
known, documented source of both idle CPU *and* flash writes on single-node
k3s, and directly relevant to this project's flash-wear goal), and
workqueue/request metrics. Every section is independent and degrades
gracefully if a tool or file isn't present (e.g. no `go` installed, or a k3s
version with a different TLS directory layout) rather than aborting the rest.
Prints a single pasteable `SUMMARY.txt` plus a full tarball for anything that
needs a deeper look.

### 5. Profile nodelet

Capture a low-overhead CPU profile from the running nodelet without restarting
it:

```bash
sudo ./deploy/profile-nodelet.sh --duration 60
```

The script writes a timestamped directory under `/tmp` containing `perf.data`,
a symbolized `perf-report.txt` when Linux `perf` is available, per-thread CPU
snapshots, and the nodelet journal for the same window. If `perf` is blocked or
not installed, it falls back to a syscall summary with `strace`.

For Rust function names, build a separate optimized binary with DWARF symbols:

```bash
cargo build --profile profiling --features cri
```

Run that binary as the process being sampled, for example:

```bash
NODELET_BIN="$PWD/target/profiling/nodelet" NODELET_RUNTIME=cri \
  ./deploy/run-nodelet.sh
```

The normal `--release` profile remains stripped for deployment. The console
summary is exclusive/self time, so a function that spends CPU in its children
does not get credited for that child work; the complete reports remain in the
output directory for call-chain inspection.

---

## Validate against containerd

An end-to-end smoke test drives the **real** CRI runtime against a live containerd —
no apiserver needed. It exercises the exact code the agent uses: `ensure_pod`
(RunPodSandbox → PullImage → CreateContainer → StartContainer), `status`, an
idempotent re-`ensure_pod`, then `remove_pod`, and asserts the container actually
reached Running and was cleaned up.

```bash
# 1. Install + start containerd (host or a privileged container):
sudo apt-get install -y containerd runc containernetworking-plugins
sudo containerd config default | sudo tee /etc/containerd/config.toml >/dev/null
# Nested/unprivileged hosts: the overlayfs snapshotter can't mount; use native:
sudo sed -i 's/snapshotter = "overlayfs"/snapshotter = "native"/' /etc/containerd/config.toml
sudo containerd &     # leave running

# 2. Build + run the smoke test (root, for socket access):
cargo build --features cri --example cri_smoke
sudo ./target/debug/examples/cri_smoke \
  unix:///run/containerd/containerd.sock docker.io/library/busybox:latest
# -> "PASS: CRI runtime validated end-to-end against containerd"
```

Notes from validation:
- This smoke test's pod uses **hostNetwork** so it needs no CNI or apiserver at
  all (it drives the CRI runtime directly). Real deployments don't have this
  restriction: `nodelet` only forces hostNetwork when a Pod spec asks for it —
  any other pod goes through containerd's normal CNI path and gets a real pod
  IP. See [Pod networking (CNI)](#pod-networking-cni) below.
- **Event-driven status has two sources, tried in order:** the CRI-standard
  `GetContainerEvents` stream (containerd ≥ 1.7, CRI-O), and — when that's
  unimplemented — containerd's own **`Events/Subscribe`** firehose (present in
  *every* containerd version). The fallback watches `/tasks/*` events and maps the
  container id back to a pod via labels. Validated against containerd 1.6.20: the
  smoke test confirms task events arrive and resolve to the right pod. So there is
  **no per-second polling on any supported containerd**.

## Testing

Two layers, and they're not substitutes for each other:

- **`cargo test [--features cri]`** — pure logic in isolation: decision
  matrices (restart/init-container/eviction-QoS-ranking), parsers (Quantity,
  dockerconfigjson, DNS config), translation tables (securityContext →
  `LinuxContainerSecurityContext`, resources → `LinuxContainerResources`).
  No cluster needed; runs anywhere, including this repo's CI.
- **`deploy/test-e2e.sh`** — functional tests against a real, already-running
  not-k8s deployment (stripped k3s + nodelet with `NODELET_RUNTIME=cri`):
  does `kubectl apply` a manifest actually converge to what it should on a
  *live* apiserver + real containerd, not just "does the code that would
  handle it look right in isolation." Set up a cluster first
  (`deploy/bootstrap-source.sh --with-cri`), export `KUBECONFIG`, then:

  ```bash
  ./deploy/test-e2e.sh                # run everything
  ./deploy/test-e2e.sh --only=probes  # only test function names containing "probes"
  ./deploy/test-e2e.sh --keep         # leave the test namespace for inspection
  ```

  **Must run on the same node as nodelet.** `kubectl exec`/`kubectl logs`
  now work for real (`crates/nodelet/src/server/`, `deploy/lib/test/cases/
  streaming.sh`) — but several other checks (resource limits,
  securityContext, hostAliases, DNS config, log rotation) still read files
  a container wrote into a shared `emptyDir` instead, or — for ConfigMap/
  Secret/downwardAPI/projected volumes — read nodelet's own materialized
  volume directly off the host filesystem at
  `/var/lib/nodelet/pods/<uid>/volumes/<name>/...`. That's not a
  workaround bolted onto the tests; it's the same path the container itself
  has bind-mounted, so it's exactly what's inside the container, and it's
  useful independent of `kubectl exec` for checking state that only
  changes because of something nodelet itself did on the host side.

  **`streaming.sh`'s `kubectl exec` test is the one to watch first** when
  you run this: `server/exec.rs`'s connection-splicing proxy was written
  without ever observing a real SPDY/WebSocket handshake (no live cluster
  in the environment that built it) — see `docs/GAP_CLOSURE.md`'s round 6
  notes. `kubectl logs` carries much higher confidence (no protocol
  upgrade involved).

  A handful of tests need extra setup and skip cleanly without it:
  `TEST_STATIC_POD_PATH` (static pods — nodelet must be running with a
  matching `NODELET_STATIC_POD_PATH`) and `TEST_LOG_MAX_SIZE_BYTES` (log
  rotation — nodelet must be running with a small
  `NODELET_CONTAINER_LOG_MAX_SIZE_BYTES` so a test can actually fill it).
  Node-pressure eviction, orphaned-sandbox GC, and graceful node shutdown
  are documented as manual procedures instead of automated tests — each
  needs either exhausting a real resource, stopping nodelet out from under
  a pod, or an actual host reboot/poweroff, none of which this suite does
  to a host you're relying on. `graceful_shutdown.sh` has the manual
  spot-check steps and is the piece to watch most closely — its D-Bus glue
  was written without ever observing a real systemd-logind (see
  `docs/GAP_CLOSURE.md`'s round 9 notes).

  `prom_metrics.sh` checks `/metrics/resource` and `/metrics/cadvisor`
  directly (same bearer-token-minting pattern as `stats.sh`) for the
  Prometheus-text HELP/TYPE lines and the running pod's labels.

  `ephemeral_containers.sh` runs `kubectl debug` against a live pod and
  checks `ephemeralContainerStatuses` — skips cleanly if the test cluster's
  kubectl/apiserver is too old to support the `ephemeralcontainers`
  subresource.

  `cgroup_hierarchy.sh` reads `/sys/fs/cgroup` directly (host state, not
  reachable through the Kubernetes API) to check the top-level `kubepods`
  cgroup exists with readable `cpu.max`/`memory.max`, and that a BestEffort
  pod's own cgroup lands somewhere findable by UID underneath it — tolerant
  of either cgroupfs or systemd driver naming. This is the piece to watch
  most closely from round 11: the actual cgroup v2 writes were never
  exercised against a real `/sys/fs/cgroup` (see `docs/GAP_CLOSURE.md`'s
  round 11 notes).

  `csi_pvc.sh` creates a PVC and a pod mounting it, then checks a file the
  container wrote lands in the host-materialized volume path. Needs
  `TEST_CSI_STORAGE_CLASS` set to a StorageClass backed by both a working
  external-provisioner and a driver also listed in the running nodelet's
  `NODELET_CSI_DRIVERS` — skips cleanly without it, since this suite can't
  stand up that infrastructure itself. This is round 12's least-validated
  piece: no CSI driver socket was reachable in the environment that built
  `runtime/csi.rs` (see `docs/GAP_CLOSURE.md`'s round 12 notes).

  `csi_plugin_registration.sh` checks the dynamic-discovery registry
  directory actually gets created, plus a manual-note for the full
  registration handshake (needs a real CSI driver's registrar pointed at
  `NODELET_PLUGIN_REGISTRY_PATH`) — `csi_pvc.sh` run *without*
  `NODELET_CSI_DRIVERS` set is the actual end-to-end proof that dynamic
  discovery works, once that's set up.

  `device_plugins.sh` checks the same shared registry directory, plus
  manual-notes for the full device-allocation flow and (round 21)
  `GetPreferredAllocation`/`PreStartContainer` specifically — this suite
  has no GPU/FPGA hardware to test against and a real device plugin
  binary isn't something to bundle here (unlike CSI, a fake gRPC device
  plugin needs no real hardware to build, so this is flagged as a natural
  next step rather than attempted so far).

  `cpu_manager.sh` creates two Guaranteed 1-CPU pods and checks their
  `cpuset.cpus` cgroup files (found by container ID, tolerant of driver
  naming) are non-empty and disjoint, plus (round 16) a second test that
  creates a BestEffort pod first and asserts its cpuset actually *changes*
  once a later Guaranteed pod claims an exclusive core — proving the
  retroactive shared-pool update, not just disjoint assignment. Unlike
  device plugins, this needs no special hardware, so it's a real automated
  check. Needs `TEST_CPU_MANAGER_STATIC=true` telling the suite the
  running nodelet has `NODELET_CPU_MANAGER_POLICY=static` set.

  `topology_manager.sh` checks that `single-numa-node` and (round 20)
  `restricted` policy both never spuriously reject a pod on a
  single-NUMA-node host (the common case — `align()` alone already
  satisfies it there), plus manual-notes for genuine cross-provider
  (CPU + device) alignment and `restricted`'s multi-node `spread()`
  fallback, both of which need real multi-socket hardware or a
  NUMA-aware device plugin. Needs
  `TEST_TOPOLOGY_MANAGER_POLICY=single-numa-node` or `=restricted`.

  `memory_manager.sh` creates a Guaranteed pod with a memory limit and
  checks its `cpuset.mems` cgroup file is non-empty (found by container
  ID, same technique `cpu_manager.sh` uses). Needs
  `TEST_MEMORY_MANAGER_STATIC=true`.

  `csi_attach.sh` provisions a PVC against an attach-requiring
  StorageClass, waits for the pod to reach Running, then asserts the
  matching `VolumeAttachment.status.attached` is `true` — proof the pod
  only started because nodelet actually waited on it. Needs
  `TEST_CSI_ATTACH_STORAGE_CLASS` set to a StorageClass backed by a driver
  that requires attach with a working external-attacher — same class of
  infra dependency `csi_pvc.sh` has, but for a driver where
  `CSIDriver.spec.attachRequired` is `true` instead of `false`.

  `readiness_gates.sh` (round 23) needs no real infrastructure at all —
  the test itself plays the "external controller" role via `kubectl patch
  --subresource=status`: creates a pod with a `readinessGates` entry,
  confirms `Ready` stays `False` while the gate condition is unset or
  `False` even though `ContainersReady` is `True`, patches it to `True`,
  confirms `Ready` flips, and confirms the gate condition itself survives
  nodelet's subsequent status reconciles (proof the foreign-condition
  carry-forward fix works, not just that `Ready` happened to flip once).
  Genuinely automatable, unlike most CSI/device-plugin/NUMA-adjacent tests
  in this suite.

  `lifecycle.sh` gained two more real automated tests (round 24, also no
  infra needed): one creates a `restartPolicy: Never` pod that exits `3`
  and asserts `containerStatuses[0].state.terminated.exitCode` is `3`
  with a non-empty `reason`; the other has a container write to
  `/dev/termination-log` before exiting and asserts the exact string
  round-trips into `state.terminated.message` — proof of both the
  termination-log bind mount and the read-back.

  `security.sh` gained a real automated test (round 25): a pod with
  `hostUsers: false` writes `/proc/self/uid_map` to a shared `emptyDir`,
  and the test asserts it does *not* show the host's own full identity
  range (`"0 0 4294967295"`) — genuine proof a user namespace is actually
  in effect. Needs a CRI runtime version that actually supports CRI's
  `userns_options` (containerd ≥ 1.7 with a matching runc build) — this
  suite can't verify that independently, so the test's failure message
  calls it out as a specific thing to check.

## Configuration (environment variables)

| Variable | Default | Meaning |
|---|---|---|
| `KUBECONFIG` | standard resolution | Points the agent at the local apiserver. |
| `NODELET_NODE_NAME` | system hostname | Node object name to register. |
| `NODELET_RUNTIME` | `mock` | `mock` or `cri` (`cri` needs `--features cri`). |
| `NODELET_CRI_ENDPOINT` | `unix:///run/containerd/containerd.sock` | CRI socket. |
| `NODELET_HEARTBEAT_SECS` | `10` | Lease renewal interval (cheap liveness). |
| `NODELET_STATUS_SECS` | `60` | Node status push interval (heavier, infrequent). |
| `NODELET_CPU` | detected nproc | Advertised CPU capacity (cores). |
| `NODELET_MEMORY_BYTES` | detected | Advertised memory capacity (bytes). |
| `NODELET_MAX_PODS` | `110` | Pod capacity. |
| `NODELET_LABELS` | — | Extra node labels, `k=v,k=v`. |
| `NODELET_SERVICE_PROXY` | `true` if `cri`, else `false` | Program ClusterIP/NodePort nftables rules (see [Services](#services-clusterip--nodeport-no-kube-proxy)). |
| `NODELET_IP_FAMILY` | `auto` | `auto`, `ipv4`, `ipv6`, or `dual` — see [IPv4 / IPv6 / dual-stack](#ipv4--ipv6--dual-stack). |
| `NODELET_LB_METHOD` | `random` | `random`, `round-robin`, or `source-hash` — see [Services](#services-clusterip--nodeport-no-kube-proxy). |
| `NODELET_MEMORY_PRESSURE_THRESHOLD_BYTES` | `104857600` (100Mi) | `MemoryPressure` fires when `/proc/meminfo` MemAvailable drops below this. |
| `NODELET_DISK_PATH` | `/var/lib/nodelet` | Filesystem path `DiskPressure` is measured against. |
| `NODELET_DISK_PRESSURE_PERCENT` | `10` | `DiskPressure` fires when available space on `NODELET_DISK_PATH` drops below this percent. |
| `NODELET_GC_INTERVAL_SECS` | `300` | How often orphaned-sandbox and unreferenced-image GC runs (`cri` only). |
| `NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT` | `85` | Image GC only starts reclaiming space once disk usage on `NODELET_DISK_PATH` reaches this percent (`cri` only). See [Status](#status). |
| `NODELET_IMAGE_GC_LOW_THRESHOLD_PERCENT` | `80` | Image removal (oldest-unreferenced first) stops once usage drops to this percent, or nothing eligible remains. |
| `NODELET_IMAGE_GC_MIN_AGE_SECS` | `120` | An unreferenced image must have been unreferenced for at least this long before it's eligible for removal. |
| `NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG` | — | Path to a `CredentialProviderConfig` YAML file (kubelet's `--image-credential-provider-config`); unset disables image credential providers entirely (`cri` only). See [Status](#status). |
| `NODELET_IMAGE_CREDENTIAL_PROVIDER_BIN_DIR` | — | Directory holding the credential-provider binaries the config's `providers[].name` refers to (kubelet's `--image-credential-provider-bin-dir`). |
| `NODELET_CLUSTER_DNS` | — | Comma-separated cluster DNS server IPs, injected into `dnsPolicy: ClusterFirst` pods (real kubelet's `--cluster-dns`). Unset means pods fall back to the host's own resolv.conf. |
| `NODELET_CLUSTER_DOMAIN` | `cluster.local` | Base domain for a ClusterFirst pod's DNS search list (`--cluster-domain`). |
| `NODELET_EVICTION_CHECK_SECS` | `10` | How often node-pressure eviction re-checks and evicts one eligible pod under pressure — see [Status](#status). |
| `NODELET_PID_PRESSURE_PERCENT` | `10` | `PIDPressure` fires when available PIDs drop below this percent. |
| `NODELET_CONTAINER_LOG_MAX_SIZE_BYTES` | `10485760` | A container's log file is rotated once it exceeds this size. |
| `NODELET_CONTAINER_LOG_MAX_FILES` | `5` | Rotated log files kept per container (including the active one). |
| `NODELET_STATIC_POD_PATH` | (none) | Directory of static Pod manifests to run on this node; unset disables static pods entirely. |
| `NODELET_STATIC_POD_SYNC_SECS` | `20` | How often the static pod manifest directory is rescanned. |
| `NODELET_SERVER_ENABLED` | `true` if `cri`, else `false` | Run the kubelet-style HTTP(S) server (`kubectl logs`/`exec`/`attach`/`port-forward`). |
| `NODELET_SERVER_PORT` | `10250` | Port for that server (real kubelet's default). |
| `NODELET_SERVER_CERT_DIR` | `/var/lib/nodelet/pki` | Where its self-signed TLS cert/key are generated/cached. |
| `NODELET_SHUTDOWN_GRACE_PERIOD_SECS` | `0` (disabled) | Total time budget to gracefully terminate pods after systemd-logind signals an imminent shutdown, before releasing the inhibitor lock (`cri` only) — see [Status](#status). |
| `NODELET_SHUTDOWN_GRACE_PERIOD_CRITICAL_SECS` | `0` | Sub-budget of the above reserved for `system-node-critical`/`system-cluster-critical` pods, terminated last; clamped to `NODELET_SHUTDOWN_GRACE_PERIOD_SECS`. |
| `NODELET_SYSTEM_RESERVED_CPU_MILLICORES` | `0` | CPU reserved for non-Kubernetes host processes, subtracted from `Node.status.allocatable` (not `capacity`) — see [Status](#status). |
| `NODELET_SYSTEM_RESERVED_MEMORY_BYTES` | `0` | Same, for memory bytes. |
| `NODELET_KUBE_RESERVED_CPU_MILLICORES` | `0` | CPU reserved for nodelet/the container runtime itself. |
| `NODELET_KUBE_RESERVED_MEMORY_BYTES` | `0` | Same, for memory bytes. |
| `NODELET_CGROUP_FS_ROOT` | `/sys/fs/cgroup` | Where the host's cgroup v2 unified hierarchy is mounted (`cri` only) — used to create/cap the top-level `kubepods` cgroup at `Node.status.allocatable`. |
| `NODELET_CSI_DRIVERS` | (none) | Comma-separated `driver-name=unix:///path/to/socket` pairs mapping a CSI driver name to its Node-service socket (`cri` only) — see [Status](#status). Unset means `PersistentVolumeClaim` volumes are skipped with a warning unless a driver registers itself dynamically (below). |
| `NODELET_PLUGIN_REGISTRY_PATH` | `/var/lib/nodelet/plugins_registry` | Directory watched for CSI driver registration sockets (`cri` only) — point a driver's `node-driver-registrar` `--kubelet-registration-path` here for dynamic discovery. |
| `NODELET_PLUGIN_REGISTRY_SYNC_SECS` | `10` | How often that directory is rescanned for new/removed sockets. |
| `NODELET_POD_RESOURCES_SOCKET_PATH` | `/var/lib/nodelet/pod-resources/kubelet.sock` | Unix socket the PodResources API's gRPC server (`List`/`GetAllocatableResources`/`Get`) binds (`cri` only) — set to the empty string to disable. See [Status](#status). |
| `NODELET_CPU_MANAGER_POLICY` | `none` | `none` or `static` (`cri` only) — pins Guaranteed-QoS containers requesting a whole number of CPUs to exclusive cores. See [Status](#status). |
| `NODELET_MEMORY_MANAGER_POLICY` | `none` | `none` or `static` (`cri` only) — pins Guaranteed-QoS containers with a memory limit to a single NUMA node. See [Status](#status). |
| `NODELET_TOPOLOGY_MANAGER_POLICY` | `none` | `none`, `best-effort`, `restricted`, or `single-numa-node` (`cri` only) — coordinates CPU Manager, Memory Manager, and device plugins by NUMA node. See [Status](#status). |
| `NODELET_MEMORY_SWAP_BEHAVIOR` | `NoSwap` | `NoSwap` or `LimitedSwap` (`cri` only) — `NoSwap` disables swap for every memory-limited container; `LimitedSwap` grants Burstable-shaped containers a proportional share of the node's swap. See [Status](#status). |
| `NODELET_USERNS_BASE_UID` | `100000` | Base host UID/GID for `spec.hostUsers: false` pods' exclusive ID ranges (`cri` only). See [Status](#status). |
| `NODELET_USERNS_LENGTH` | `65536` | Size of each pod's exclusive UID/GID range (`cri` only). |
| `NODELET_USERNS_MAX_PODS` | `1024` | How many concurrent `hostUsers: false` pods this node's allocator supports (`cri` only). |
| `RUST_LOG` | `info` | Tracing filter, e.g. `nodelet=debug`. |

---

## License

MIT OR Apache-2.0.
