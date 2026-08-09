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

Measured idle, with no pods scheduled, a standalone upstream kubelet sits at
roughly **81 MB RSS** and **~0.85s of CPU time per 2-minute window**, against
`nodelet`'s **~15 MB** and **~0.08s**. Those numbers come from
`.github/workflows/profiling.yml`, which runs both agents on GitHub-hosted
`ubuntu-latest` x86_64 runners — three replicates each, in parallel, one
agent per runner — not on any edge device. See the
[`profiling-results`](https://github.com/centerionware/not-k8s/tree/profiling-results)
branch for the raw per-second data and methodology. Equivalent numbers on
ARM/edge hardware haven't been published; releases build for
x86_64/aarch64/armv7l, but only x86_64 has been profiled.

## Why the Node Agent's Floor Matters

On a machine with 64 GB of RAM, saving 66 MB per node is a rounding error.
Across a large fleet it adds up to something real but still modest — some
reclaimed scheduling capacity, some CPU cycles handed back to workloads.
That's a fine reason to prefer it, not a reason to build it.

The bigger reason is what the node agent's resource floor decides: **which
hardware can be a Kubernetes node at all.**

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

### Its own Service networking, not a separate kube-proxy

`nodelet` watches Services/Endpoints and programs ClusterIP/NodePort
routing itself via nftables (`crates/nodelet/src/svc.rs`) — no separate
kube-proxy process, no separate periodic iptables sync pass. (With the
`mock` runtime there's no real networking to route to, so this only
applies under `cri`.)

### Single process, single watch cache

Stock kubelet's node footprint is kubelet + kube-proxy + containerd +
containerd-shim(s), each maintaining its own informer/watch cache —
duplicated in-memory copies of the same API objects. `nodelet` is **one
process** with **one kube client** and **one watch cache**. Memory scales
with the number of pods on this node, not the number of node-side
components.

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
│  ┌──────────────┐  ┌─────────────┐  ┌────────────────────┐   │
│  │ svc.rs        │  │ Static pod  │  │ kubelet-style HTTPS  │   │
│  │ (Service      │  │ manifest    │  │ server (exec/attach/ │   │
│  │  routing via  │  │ watcher     │  │  portForward/logs)   │   │
│  │  nftables)    │  │             │  │                       │   │
│  └──────────────┘  └─────────────┘  └────────────────────┘   │
└───────────────────────────┬─────────────────────────────────┘
                            │ HTTPS (kubeconfig)
                            │ - Node registration + Lease heartbeat
                            │ - Watch Pods (fieldSelector: spec.nodeName)
                            │ - Patch Pod status
                            ▼
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
