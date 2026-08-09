# nodelet: Architecture

> Replace kubelet with a lean, event-driven Rust node agent that speaks the
> same narrow contract to whatever control plane it's pointed at — so the
> hardware that can be a Kubernetes node isn't limited to what a datacenter
> considers a server.

---

## Table of Contents

1. [What nodelet Is](#what-nodelet-is)
2. [Why kubelet Is Expensive at Idle](#why-kubelet-is-expensive-at-idle)
3. [Why the Node Agent's Floor Matters](#why-the-node-agents-floor-matters)
4. [Design: Event-Driven, Not Polled](#design-event-driven-not-polled)
5. [Internal Architecture](#internal-architecture)
6. [Pluggable Runtime: mock vs cri](#pluggable-runtime-mock-vs-cri)
7. [Configuration Reference](#configuration-reference)
8. [Scope Boundary](#scope-boundary)
9. [Current Status](#current-status)

---

## What nodelet Is

`nodelet` is a drop-in replacement for kubelet, the per-node agent in a
Kubernetes cluster. It talks to whatever control plane it's pointed at
(this project develops and tests against k3s, but nothing about nodelet's
own design assumes k3s specifically) through the same narrow interface real
kubelet uses:

- Register a `Node` object.
- Maintain a `Lease` (heartbeat).
- Watch `Pod`s with `fieldSelector=spec.nodeName=<self>`.
- Patch `Pod` status back.
- Report `Node` status/conditions.
- Run a kubelet-style HTTPS server for `exec`/`attach`/`portForward`/logs.

Nothing on the other side of that contract needs to know or care that
`nodelet` isn't kubelet. This document is about what happens *inside*
nodelet — not about the control plane it talks to.

## Why kubelet Is Expensive at Idle

Stock kubelet does real work even when a node has zero pods to run, almost
entirely from polling loops that were designed for correctness/simplicity,
not idle efficiency:

| Source | What it does at idle | Cost |
|--------|---------------------|------|
| **PLEG (Pod Lifecycle Event Generator)** | Relists all containers every 1s via CRI to detect state changes | Constant CPU even with zero pods |
| **cAdvisor housekeeping** | Scrapes cgroup stats, filesystem stats, network stats every 10-15s per container | CPU + memory for stats caches |
| **kube-proxy / iptables sync** | Periodic iptables rule reconciliation, even with no Service changes | CPU spikes every 30s |
| **Per-process watch caches** | kubelet and kube-proxy each maintain their own in-memory representation of the objects they care about | Multiplied RSS |

Measured idle, no pods scheduled, sampling every second over a 120s window,
3 replicates per agent on each platform:

| | x86_64 (CI) | ARM phone |
|---|---|---|
| **nodelet** RSS | ~15 MB | **12.0 MB** (11.8–12.1) |
| **kubelet** RSS | ~81 MB | **67.9 MB** (65.5–72.3) |
| RSS ratio | ~5.4x | **~5.7x** |
| **nodelet** CPU-sec | ~0.08s | **0.436s** (0.393–0.461) |
| **kubelet** CPU-sec | ~0.85s | **8.031s** (7.778–8.170) |
| CPU ratio | ~10.6x | **~18.4x** |

x86_64 figures come from `.github/workflows/profiling.yml` on GitHub-hosted
`ubuntu-latest` runners, 6 legs in parallel. ARM figures come from a manual
run of the same tooling on a Google Pixel 7 (Tensor G2, Cortex-X1 +
Cortex-A55, aarch64, 1.9 GB guest under KVM) — sequential legs rather than
parallel, since there is only one device. Both publish full per-second CSVs:
[x86_64](https://github.com/centerionware/not-k8s/tree/profiling-results/latest),
[ARM phone](https://github.com/centerionware/not-k8s/tree/profiling-results/history/2026-08-09_00-59-17-arm64-phone)
(that report states its own methodological limits — sequential measurement,
thermal throttling, virtualization — up front).

**Read the CPU number as absolute work, not as a percentage.** 0.85s per
120s window is well under 1% of a fast x86_64 core, and easy to dismiss on
that basis. But it's a fixed quantity of work divided by whatever core is
available, and the division gets worse as the core gets weaker. The ARM
measurements bear that out and then some: **the CPU gap widens from ~10.6x
to ~18.4x**, meaning kubelet degrades harder than `nodelet` does on a slow
core rather than both scaling by the same factor. The RSS ratio stays
nearly flat (~5.4x to ~5.7x), which is what you'd expect — resident memory
isn't a function of clock speed.

**Open question: why does the gap widen?** Normalized against their own
x86_64 baselines, `nodelet` is ~5.5x slower on the phone core while kubelet
is ~9.5x slower — so something superlinear penalizes kubelet specifically.
At least three mechanisms predict that signature, and this run cannot
distinguish them:

- **Cache pressure.** kubelet's working set is several times `nodelet`'s
  (~68 MB vs ~12 MB RSS). A phone SoC's caches are far smaller than a
  server's, so a large working set that mostly fits on the server may not
  fit here.
- **In-order execution.** The Cortex-A55 is in-order and cannot hide
  memory-stall latency the way the server's out-of-order core does. This
  doesn't compete with the cache explanation so much as multiply it.
- **Garbage collection.** kubelet is Go with a GC that periodically walks
  the heap; `nodelet` is Rust with none. GC is bandwidth-hungry, and phone
  memory bandwidth is much lower.

Settling this needs hardware performance counters (IPC, cache-miss rates),
and **the PMU is not exposed to this KVM guest** — `perf` reports every
hardware event as `<not supported>` even as root, so only software
counters like `task-clock` work. The same limitation applies to GitHub's
hosted runners. Anyone able to run this on bare-metal ARM with working
`perf` counters would be able to answer it; contributions welcome.

**On that phone, the control plane costs more than either node agent.** The
stripped `k3s server --disable-agent` control plane measured **33.2–36.2%
of a core** (~41.5 CPU-seconds per 120s window) and ~350–370 MB RSS in
every leg, regardless of which agent ran beside it — roughly 5x kubelet's
own CPU cost and ~95x `nodelet`'s. Replacing the node agent is a real
saving that does not touch the larger item on the bill. That's out of
scope for this project today, and worth stating plainly rather than
letting the node-agent numbers imply otherwise.

## Why the Node Agent's Floor Matters

On a machine with 64 GB of RAM and fast cores, saving 66 MB and a fraction
of a percent of a core per node is a rounding error. Across a large fleet
it adds up to something real but still modest — some reclaimed scheduling
capacity, some CPU cycles handed back to workloads. That's a fine reason to
prefer it, not a reason to build it.

The bigger reason is what the node agent's resource floor decides: **which
hardware can be a Kubernetes node at all.**

Both halves of that floor get worse as the hardware gets smaller, and
neither scales the way a datacenter operator's intuition expects. A fixed
~81 MB is 0.1% of a 64 GB server and over 15% of a 512 MB device. And the
polling doesn't get cheaper because the core is weaker — it gets
proportionally more expensive. That isn't a hypothesis: measured on the
phone above, kubelet's idle CPU cost rises from under 1% of a core to
**~6.6%**, while `nodelet` stays at **~0.36%**, widening the gap from
~10.6x to ~18.4x. On battery-powered or passively cooled hardware that
also competes for thermal headroom the workload needs.

Kubernetes' API is genuinely good at what a lot of small-hardware fleets
need — declarative rollouts, health checking, restart policy, secret and
config distribution, node labels and scheduling constraints, a real
permission model. People running racks of SBCs, industrial gateways, kiosks,
retired handsets, or anything else with a CPU and a network link mostly
end up reinventing worse versions of exactly that, because the orchestrator
that already solved it assumes every node can spare hundreds of megabytes
before running a single container.

That assumption is a property of the node agent, not of Kubernetes. The
control plane can live somewhere else entirely — a server, a VM, a cloud
instance — while the node agent is the only part that has to run on the
constrained device. Take it from ~81 MB to ~15 MB and a class of hardware
that couldn't participate suddenly can, without giving up `kubectl`, CRDs,
or any of the ecosystem built on them.

Kubernetes gets treated as datacenter-only infrastructure. Most of what
makes it feel that way is the node side's cost, and that part is fixable.

## Design: Event-Driven, Not Polled

The fix isn't a faster kubelet — it's removing the polling loops entirely.
Each one below is replaced by something that only wakes up when there's
actually work to do.

### Event-driven reconciliation, no PLEG

Stock kubelet's PLEG relists all containers every second via `ListPodSandbox`
+ `ListContainers`, diffs against previous state, and generates events —
constant CPU even with zero running containers.

`nodelet` instead holds a single long-lived watch on Pods bound to its node.
Reconciliation (`ensure_pod()`) only runs in response to a watch event on
that Pod object itself (or a referenced ConfigMap/Secret changing) — there
is no periodic "resync everything" loop. At idle, this is one open TCP
connection with zero CPU cost. (One consequence: anything that changes a
pod's real state *without* a Pod object mutation — a probe-triggered
restart is the example that surfaced this for real, see
`docs/E2E_FINDINGS.md` finding #19 — has to explicitly re-trigger
`ensure_pod()` itself, since nothing else will.)

### CRI event stream instead of PLEG

When using the `cri` runtime, nodelet subscribes to containerd's own event
stream (`/containerd.services.events.v1.Events/Subscribe`) for container
lifecycle notifications. State changes are pushed, not polled.

### On-demand stats instead of cAdvisor

Stock kubelet embeds cAdvisor, running housekeeping loops every 10-15s per
container regardless of whether anyone's asking for stats. `nodelet` reads
cgroup v2 stats and PSI (Pressure Stall Information) **on demand only** —
when a status update is due or a stats endpoint is actually queried. No
background housekeeping loop exists to disable.

### Decoupled heartbeat and status push

`nodelet` separates two concerns stock kubelet conflates into one push
every 10s:

- **Lease heartbeat** (`NODELET_HEARTBEAT_SECS`, default 10s) — a tiny
  (~200 byte) `Lease` PUT that says "I'm alive."
- **Node status push** (`NODELET_STATUS_SECS`, default 60s) — the larger
  update with capacity, conditions, allocatable resources. Infrequent
  because that data rarely changes on a running node.

### Service networking: nftables and event-driven, in a separate binary

Service routing is `nodeproxy` (`crates/nodeproxy/`), not `nodelet`. It
watches Services + EndpointSlices and rebuilds one `inet not_k8s_svc`
nftables table atomically on every change — **no periodic resync pass at
all**, which is the substantive difference from stock kube-proxy, whose
iptables sync loop reconciles on a timer whether or not anything changed.
That claim is about the reconciliation model, not about process count.

It's a separate binary for the same reason kube-proxy is separate from the
kubelet upstream: service handling is a replaceable concern. A node can run
Cilium's eBPF datapath, a real kube-proxy, or nothing at all
(`--proxy=none`) and the node agent is unaffected — and conversely, a
wedged service proxy doesn't take the node agent down with it. This was
in-process inside `nodelet` until the split; the honest accounting of what
that cost is in the next section.

(With the `mock` runtime there's no real networking to route to, so the
deploy scripts don't install `nodeproxy` at all there.)

### One watch cache per concern, not per component

Stock kubelet's node footprint is kubelet + kube-proxy + containerd +
containerd-shim(s), each maintaining its own informer/watch cache —
duplicated in-memory copies of the same API objects. `nodelet` collapses
the node-agent side of that into **one process** with **one kube client**
and **one watch cache** over Pods and their referenced objects.

Splitting `nodeproxy` out does add a second process with its own client and
its own watch over Services/EndpointSlices — so this is no longer "one
process, one cache" full stop, and pretending otherwise would be dishonest.
What it isn't is a duplicate: the two processes watch **disjoint** resource
sets, so nothing is cached twice. The cost is one more small process's
baseline; the return is that either component can be swapped or fail
independently. Whether that trade is worth it on a given device is
measurable — `nodeproxy` is deliberately built from a minimal dependency
tree (no CRI/gRPC stack at all; see `crates/nodeproxy/Cargo.toml`) to keep
that baseline small.

## Internal Architecture

```
┌───────────────────────────────────────────────────────────────┐
│                          nodelet                               │
│                    (single Rust binary)                        │
│                                                                 │
│  ┌──────────────┐  ┌─────────────┐  ┌────────────────────┐   │
│  │ Node          │  │ Pod watcher │  │ Runtime trait       │   │
│  │ registration  │  │ (event-     │  │  ┌────────┐        │   │
│  │ + Lease       │  │  driven,    │  │  │ mock   │        │   │
│  │ heartbeat     │  │  reconcile  │  │  │ cri    │        │   │
│  │               │  │  on watch   │  │  └────────┘        │   │
│  │ Lease: 10s    │  │  events     │  │                     │   │
│  │ Status: 60s   │  │  only)      │  │                     │   │
│  └──────────────┘  └─────────────┘  └────────────────────┘   │
│                                                                 │
│  ┌──────────────┐  ┌─────────────┐  ┌────────────────────┐   │
│  │ CPU / Memory  │  │ Eviction    │  │ Plugin registry     │   │
│  │ / Topology    │  │ manager     │  │ (CSI / device       │   │
│  │ managers      │  │ (Memory/    │  │  plugins / DRA,     │   │
│  │ (cri only)    │  │  Disk/PID   │  │  one shared         │   │
│  │               │  │  pressure)  │  │  socket protocol)   │   │
│  └──────────────┘  └─────────────┘  └────────────────────┘   │
│                                                                 │
│                                                                 │
│                      ┌─────────────┐  ┌────────────────────┐   │
│                      │ Static pod  │  │ kubelet-style HTTPS  │   │
│                      │ manifest    │  │ server (exec/attach/ │   │
│                      │ watcher     │  │  portForward/logs)   │   │
│                      └─────────────┘  └────────────────────┘   │
└───────────────────────────┬─────────────────────────────────┘
                            │ HTTPS (kubeconfig)
                            │ - Node registration + Lease heartbeat
                            │ - Watch Pods (fieldSelector: spec.nodeName)
                            │ - Patch Pod status
                            │
┌─────────────────────────┐ │
│        nodeproxy         │ │  Separate binary, separate service,
│  (separate Rust binary)  │ │  no ordering between the two. Replace
│                          │ │  it with Cilium / a real kube-proxy,
│  ┌────────────────────┐ │ │  or run none at all (--proxy=none).
│  │ svc.rs             │ │ │
│  │ Service +          │ │ │
│  │ EndpointSlice      │ │ │
│  │ watch -> one       │ │ │
│  │ nftables table,    │ │ │
│  │ rebuilt atomically │ │ │
│  │ per event          │ │ │
│  └────────────────────┘ │ │
└────────────┬────────────┘ │
             │ HTTPS (kubeconfig)
             │ - Watch Services + EndpointSlices (cluster-wide)
             │   (disjoint from nodelet's watches — nothing cached twice)
             ▼             ▼
              [ Kubernetes control plane — any conformant one ]
```

Almost everything above the `Runtime` trait boundary is runtime-agnostic —
it works against `PodRuntime`, not against containerd directly. Only the
`cri` feature's implementation (`runtime/cri/`) talks to a real container
runtime; the rest of nodelet doesn't know or care which `Runtime`
implementation is plugged in.

## Pluggable Runtime: mock vs cri

```rust
trait PodRuntime {
    async fn run_pod(&self, pod: &Pod) -> Result<PodStatus>;
    async fn stop_pod(&self, pod: &Pod) -> Result<()>;
    async fn pod_status(&self, pod: &Pod) -> Result<PodStatus>;
    // ...
}
```

### mock runtime (default)

- No container engine needed — tracks pod state in memory and immediately
  reports containers as `Running`.
- Purpose: fast builds and pure-logic testing without needing a container
  runtime available.
- Build: `cargo build -p nodelet` (no feature flags). Use:
  `NODELET_RUNTIME=mock` (default).

### cri runtime

- Connects to containerd over the CRI gRPC socket: pulls images, creates
  sandboxes, starts containers, subscribes to containerd's event stream
  for lifecycle updates.
- Build: `cargo build --release --features cri -p nodelet`. Use:
  `NODELET_RUNTIME=cri` with
  `NODELET_CRI_ENDPOINT=unix:///run/containerd/containerd.sock`.

## Configuration Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `KUBECONFIG` | Standard kube client resolution | Path to kubeconfig pointing at the control plane's apiserver |
| `NODELET_NODE_NAME` | System hostname | Node name registered with the apiserver |
| `NODELET_RUNTIME` | `mock` | `mock` (in-memory, no container engine) or `cri` (containerd) |
| `NODELET_CRI_ENDPOINT` | `unix:///run/containerd/containerd.sock` | CRI gRPC socket path (cri runtime only) |
| `NODELET_HEARTBEAT_SECS` | `10` | Lease renewal interval in seconds |
| `NODELET_STATUS_SECS` | `60` | Node status push interval in seconds |
| `NODELET_CPU` | Detected via `nproc` | Advertised CPU capacity (whole cores) |
| `NODELET_MEMORY_BYTES` | Detected from `/proc/meminfo` | Advertised memory capacity in bytes |
| `NODELET_MAX_PODS` | `110` | Maximum pod capacity advertised to the scheduler |
| `NODELET_LABELS` | (none) | Extra node labels, comma-separated `key=value` pairs |
| `NODELET_MEMORY_PRESSURE_THRESHOLD_BYTES` | `104857600` (100Mi) | `MemoryPressure` fires when `/proc/meminfo` MemAvailable drops below this |
| `NODELET_DISK_PATH` | `/var/lib/nodelet` | Filesystem path `DiskPressure` is measured against |
| `NODELET_DISK_PRESSURE_PERCENT` | `10` | `DiskPressure` fires when available space on `NODELET_DISK_PATH` drops below this percent |
| `NODELET_GC_INTERVAL_SECS` | `300` | How often orphaned sandbox/container and unreferenced-image GC runs (cri runtime only) |
| `NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT` | `85` | Image GC only starts reclaiming space once `NODELET_DISK_PATH` usage reaches this percent (cri runtime only) |
| `NODELET_IMAGE_GC_LOW_THRESHOLD_PERCENT` | `80` | Image removal (oldest-unreferenced first) stops once usage drops to this percent, or nothing eligible remains |
| `NODELET_IMAGE_GC_MIN_AGE_SECS` | `120` | An unreferenced image must have been unreferenced for at least this long before it's eligible for removal |
| `NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG` | (none) | Path to a `CredentialProviderConfig` YAML file; empty disables image credential providers entirely (cri runtime only) |
| `NODELET_IMAGE_CREDENTIAL_PROVIDER_BIN_DIR` | (none) | Directory containing the credential-provider binaries named by the config's `providers[].name` |
| `NODELET_CLUSTER_DNS` | (none) | Comma-separated cluster DNS server IPs for `dnsPolicy: ClusterFirst` pods |
| `NODELET_CLUSTER_DOMAIN` | `cluster.local` | Base domain for a ClusterFirst pod's DNS search list |
| `NODELET_EVICTION_CHECK_SECS` | `10` | How often node-pressure eviction re-checks MemoryPressure/DiskPressure/PIDPressure and evicts one eligible pod if any is active |
| `NODELET_PID_PRESSURE_PERCENT` | `10` | `PIDPressure` fires when available PIDs (`pid_max` minus running processes) drop below this percent |
| `NODELET_CONTAINER_LOG_MAX_SIZE_BYTES` | `10485760` (10Mi) | A running container's log file is rotated once it exceeds this size |
| `NODELET_CONTAINER_LOG_MAX_FILES` | `5` | How many rotated log files (including the active one) survive per container |
| `NODELET_LOG_ROTATE_INTERVAL_SECS` | `10` | How often the log-rotation check runs |
| `NODELET_STATIC_POD_PATH` | (none) | Directory of static Pod manifests to run directly on this node (real kubelet's `staticPodPath`); unset disables the feature entirely |
| `NODELET_STATIC_POD_SYNC_SECS` | `20` | How often the static pod manifest directory is rescanned |
| `NODELET_SERVER_ENABLED` | `true` if `cri`, else `false` | Run the kubelet-style HTTP(S) server (containerLogs/exec/attach/portForward) |
| `NODELET_SERVER_PORT` | `10250` | Port for that server |
| `NODELET_SERVER_CERT_DIR` | `/var/lib/nodelet/pki` | Where its self-signed TLS cert/key are generated/cached |
| `NODELET_SHUTDOWN_GRACE_PERIOD_SECS` | `0` (disabled) | Total time budget to gracefully terminate pods after systemd-logind signals an imminent shutdown, before releasing the inhibitor lock (cri runtime only) |
| `NODELET_SHUTDOWN_GRACE_PERIOD_CRITICAL_SECS` | `0` | Sub-budget of the above reserved for `system-node-critical`/`system-cluster-critical` pods, terminated last; clamped to `NODELET_SHUTDOWN_GRACE_PERIOD_SECS` |
| `NODELET_SYSTEM_RESERVED_CPU_MILLICORES` | `0` | CPU reserved for non-Kubernetes host processes, subtracted from `Node.status.allocatable` (not `capacity`) |
| `NODELET_SYSTEM_RESERVED_MEMORY_BYTES` | `0` | Same, for memory bytes |
| `NODELET_KUBE_RESERVED_CPU_MILLICORES` | `0` | CPU reserved for nodelet/the container runtime itself |
| `NODELET_KUBE_RESERVED_MEMORY_BYTES` | `0` | Same, for memory bytes |
| `NODELET_CGROUP_FS_ROOT` | `/sys/fs/cgroup` | Where the host's cgroup v2 unified hierarchy is mounted (cri runtime only) — used to create/cap the top-level `kubepods` cgroup at `Node.status.allocatable` |
| `NODELET_CSI_DRIVERS` | (none) | Comma-separated `driver-name=unix:///path/to/socket` pairs mapping a CSI driver name to its Node-service socket (cri runtime only). Unset means PersistentVolumeClaim volumes are skipped with a warning unless a driver registers itself dynamically |
| `NODELET_PLUGIN_REGISTRY_PATH` | `/var/lib/nodelet/plugins_registry` | Directory watched for CSI driver registration sockets (cri runtime only) — point a driver's node-driver-registrar `--kubelet-registration-path` here for dynamic discovery |
| `NODELET_PLUGIN_REGISTRY_SYNC_SECS` | `10` | How often that directory is rescanned for new/removed sockets |
| `NODELET_POD_RESOURCES_SOCKET_PATH` | `/var/lib/nodelet/pod-resources/kubelet.sock` | Unix socket the PodResources API's gRPC server (`List`/`GetAllocatableResources`/`Get`) binds (cri runtime only) — set empty to disable; point external device-monitoring tooling here |
| `NODELET_CPU_MANAGER_POLICY` | `none` | `none` or `static` (cri runtime only) — pins Guaranteed-QoS containers requesting a whole number of CPUs to exclusive cores |
| `NODELET_MEMORY_MANAGER_POLICY` | `none` | `none` or `static` (cri runtime only) — pins Guaranteed-QoS containers with a memory limit to a single NUMA node |
| `NODELET_TOPOLOGY_MANAGER_POLICY` | `none` | `none`, `best-effort`, `restricted`, or `single-numa-node` (cri runtime only) — coordinates CPU Manager, Memory Manager, and device plugins by NUMA node |
| `NODELET_MEMORY_SWAP_BEHAVIOR` | `NoSwap` | `NoSwap` or `LimitedSwap` (cri runtime only) — `NoSwap` pins every memory-limited container's swap ceiling to its own memory limit (zero additional swap); `LimitedSwap` grants Burstable-shaped containers a proportional swap share per KEP-2400's formula (`request / node memory * node swap`) |
| `NODELET_USERNS_BASE_UID` | `100000` | Base host UID/GID for `spec.hostUsers: false` pods' exclusive ID ranges (cri runtime only) |
| `NODELET_USERNS_LENGTH` | `65536` | Size of each pod's exclusive UID/GID range (cri runtime only) |
| `NODELET_USERNS_MAX_PODS` | `1024` | How many concurrent `hostUsers: false` pods this node's allocator supports (cri runtime only) |
| `RUST_LOG` | (none) | Tracing filter, e.g. `info`, `nodelet=debug,kube=warn` |

## Scope Boundary

`nodelet` replaces kubelet: it consumes the same API a real kubelet would,
nothing more. `nodelet` has no architectural assumption limiting it to one
node — each device runs its own independent `nodelet` instance and
registers its own `Node` object, exactly like a real kubelet would, so
many `nodelet`-managed nodes can point at one shared control plane with
the stock scheduler binding pods across all of them normally. A single
edge device is the case this project optimizes and tests hardest against,
but the goal is a drop-in kubelet replacement usable in ordinary
multi-node clusters too, not a single-node-only tool.

## Current Status

`not-k8s` is alpha software — it hasn't been proven under heavy real-world
production workloads yet. What it does have: 1,100+ unit tests and ~140
end-to-end tests exercising containerd plus CSI/DRA reference drivers,
run as a required gate before any release ships (see the
[Actions history](https://github.com/centerionware/not-k8s/actions), not
this document, for actual pass/fail results). Feature-by-feature parity
against upstream kubelet is tracked as an ongoing, actively-updated effort
rather than claimed once and left stale — grep the codebase or check recent
commits before assuming something is or isn't implemented, since this
document describes architecture, not a feature-completeness snapshot.
