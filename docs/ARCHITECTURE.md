# nodelet: Architecture

> Replace kubelet — and only kubelet — with a lean, event-driven Rust node
> agent that speaks the same narrow contract to whatever control plane it's
> pointed at.

---

## Table of Contents

1. [What nodelet Is](#what-nodelet-is)
2. [Why kubelet Is Expensive at Idle](#why-kubelet-is-expensive-at-idle)
3. [Design: Event-Driven, Not Polled](#design-event-driven-not-polled)
4. [Internal Architecture](#internal-architecture)
5. [Pluggable Runtime: mock vs cri](#pluggable-runtime-mock-vs-cri)
6. [Configuration Reference](#configuration-reference)
7. [Scope Boundary](#scope-boundary)
8. [Current Status](#current-status)

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
| **Per-process watch caches** | kubelet, kube-proxy, and each containerd shim maintain their own in-memory representation of the objects they care about | Multiplied RSS |
| **containerd + shim processes** | Container runtime overhead even when idle | RSS baseline ~30-50 MB |

On a Raspberry Pi 4 (4 GB RAM, 4-core ARM), stock kubelet alone idles around
**81 MB RSS** and **~0.85s of CPU time per 2-minute idle window** — see the
[`profiling-results`](https://github.com/centerionware/not-k8s/tree/profiling-results)
branch for live, CI-published numbers against a real upstream kubelet binary.

## Design: Event-Driven, Not Polled

The fix isn't a faster kubelet — it's removing the polling loops entirely.
`nodelet` idles at **~15 MB RSS** and **~0.08s of CPU time** over the same
window by replacing each polling loop with something that only wakes up
when there's actually work to do.

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
- Purpose: isolate and measure nodelet's own control-loop overhead without
  any container-engine noise. This is what the idle-CPU/RSS numbers above
  measure against a real kubelet binary — same comparison, mock runtime on
  the nodelet side, so the delta is architecture, not container-engine
  differences.
- Build: `cargo build -p nodelet` (no feature flags). Use:
  `NODELET_RUNTIME=mock` (default).

### cri runtime

- Connects to containerd over the CRI gRPC socket: pulls images, creates
  sandboxes, starts real containers, subscribes to containerd's event
  stream for lifecycle updates.
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

`nodelet` replaces kubelet — nothing upstream of the apiserver. Concretely,
out of scope:

- **Everything the scheduler, controller-manager, and apiserver itself do**
  — binding pods to nodes, admission, RBAC, server-side apply field
  ownership, CRD storage/conversion, discovery. `nodelet` doesn't
  reimplement any of it; it consumes the same API a real kubelet would.
- **Control-plane high availability.** Whatever HA story the control plane
  uses is orthogonal to nodelet. **This is a separate axis from multi-node
  support**: `nodelet` has no architectural assumption limiting it to one
  node. Each device runs its own independent `nodelet` instance and
  registers its own `Node` object, exactly like a real kubelet would;
  nothing stops many `nodelet`-managed nodes pointing at one shared control
  plane, with the stock scheduler binding pods across all of them normally.
  A single edge device is the case this project optimizes and tests
  hardest against — but the goal is a drop-in kubelet replacement usable in
  ordinary multi-node clusters too, not a single-node-only tool.
- **Mesh federation sync agent.** Devices federating to an upstream cluster
  over Tailscale/Netbird, syncing selected resources bidirectionally, is a
  separate future component, not part of nodelet's current architecture.
- **Windows / non-Linux.** cgroup v2, `/proc`, and the CRI socket are
  Linux-specific.

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
