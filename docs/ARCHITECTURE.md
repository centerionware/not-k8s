# not-k8s: Architecture

> Make a single edge device behave like a self-contained, low-overhead Kubernetes
> node that accepts normal `kubectl apply` / CRDs and runs workloads offline.

---

## Table of Contents

1. [The Problem](#the-problem)
2. [The Thesis](#the-thesis)
3. [Architecture Overview](#architecture-overview)
4. [Why Replace the Node Side, Not the API Server](#why-replace-the-node-side-not-the-api-server)
5. [How nodelet Stays Cheap](#how-nodelet-stays-cheap)
6. [Pluggable Runtime: mock vs cri](#pluggable-runtime-mock-vs-cri)
7. [Configuration Reference](#configuration-reference)
8. [Out of Scope](#out-of-scope)
9. [Roadmap](#roadmap)
10. [Limitations and Status](#limitations-and-status)

---

## The Problem

Kubernetes — even "lightweight" distributions like k3s and k0s — carries
significant idle overhead.  On a resource-constrained edge device (a phone, an
SBC, a field gateway), that overhead dominates the CPU and memory budget before
you run a single workload.

Concrete sources of idle cost in a stock single-node k3s/k8s setup:

| Source | What it does at idle | Cost |
|--------|---------------------|------|
| **PLEG (Pod Lifecycle Event Generator)** | Relists all containers every 1s via CRI to detect state changes | Constant CPU even with zero pods |
| **cAdvisor housekeeping** | Scrapes cgroup stats, filesystem stats, network stats every 10-15s per container | CPU + memory for stats caches |
| **etcd / kine** | Maintains consistent datastore; compaction, defrag, snapshot cycles | Disk I/O + memory for mvcc store |
| **Leader-election lease churn** | kube-scheduler + controller-manager renew leases every 2s | Steady API write traffic |
| **Informer resyncs** | Each controller resyncs its full object list periodically (default 0-12h, but list calls are expensive) | Memory spikes, API load |
| **Watch caches (per-process)** | Each component (kubelet, kube-proxy, scheduler, CM) maintains its own in-memory watch cache | Multiplied RSS |
| **containerd + shim processes** | Container runtime overhead even when idle | RSS baseline ~30-50 MB |
| **kube-proxy / iptables sync** | Periodic iptables rule reconciliation | CPU spikes every 30s |

On a Raspberry Pi 4 (4 GB RAM, 4-core ARM), a stock k3s install at idle
consumes roughly **150-400 MB RSS** and **30-50% of a single core**.  That's
before running any user workload.

## The Thesis

> It's **architecture**, not language — but a GC-free, event-driven Rust node
> agent plus a stripped real control plane gets the wins while staying 1:1
> kubectl/CRD compatible.

The insight is that most of the idle cost lives on the **node side** (kubelet,
PLEG, cAdvisor, kube-proxy, containerd shims), not in the apiserver.  The
apiserver itself is relatively efficient at idle — it's a watch-multiplexer
backed by a datastore.

By keeping the real apiserver (via k3s) and replacing *only* the node agent
with a purpose-built binary, we:

- Preserve **100% kubectl and CRD compatibility** for free — no reimplementation
  of discovery, OpenAPI, RBAC, admission webhooks, or server-side apply.
- Eliminate the **major idle-cost sources**: PLEG polling, cAdvisor, kube-proxy
  iptables sync, per-component watch caches.
- Get a **single-process, single-watch-cache** node agent that does exactly
  what's needed and nothing more.

Rust is a means, not the end.  The real win is architectural: event-driven
reconciliation instead of periodic polling.  Rust makes it practical to write
that agent with zero GC pauses, minimal RSS, and no runtime overhead.

## Architecture Overview

```
┌───────────────────────────────────────────────────────────┐
│                    Edge Device                            │
│                                                           │
│  ┌─────────────────────────────────────────────────────┐  │
│  │            k3s server (--disable-agent)              │  │
│  │                                                     │  │
│  │  ┌──────────┐  ┌───────────┐  ┌──────────────────┐ │  │
│  │  │apiserver │  │ scheduler │  │ controller-manager│ │  │
│  │  │          │  │           │  │                    │ │  │
│  │  │ OpenAPI  │  │ binds Pods│  │ node-monitor      │ │  │
│  │  │ RBAC     │  │ to Node   │  │ replication        │ │  │
│  │  │ admission│  │           │  │ garbage collection │ │  │
│  │  └────┬─────┘  └───────────┘  └──────────────────┘ │  │
│  │       │                                             │  │
│  │       │  kine/SQLite (embedded datastore)           │  │
│  │       │  (replaces etcd — single-node, no quorum)   │  │
│  └───────┼─────────────────────────────────────────────┘  │
│          │                                                │
│          │ HTTPS (kubeconfig)                             │
│          │ - Watch Pods (fieldSelector: spec.nodeName)    │
│          │ - Node registration + status updates           │
│          │ - Lease heartbeat                              │
│          │                                                │
│  ┌───────┴─────────────────────────────────────────────┐  │
│  │                    nodelet                           │  │
│  │              (single Rust binary)                    │  │
│  │                                                     │  │
│  │  ┌──────────────┐  ┌─────────────┐  ┌───────────┐ │  │
│  │  │ Node         │  │ Pod watcher │  │ Runtime   │ │  │
│  │  │ registration │  │ (event-     │  │ trait     │ │  │
│  │  │ + heartbeat  │  │  driven)    │  │           │ │  │
│  │  │              │  │             │  │ ┌───────┐ │ │  │
│  │  │ Lease: 10s   │  │ reconcile   │  │ │ mock  │ │ │  │
│  │  │ Status: 60s  │  │ on watch    │  │ │ cri   │ │ │  │
│  │  │              │  │ events only │  │ └───────┘ │ │  │
│  │  └──────────────┘  └─────────────┘  └───────────┘ │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                           │
│  ┌─────────────────────────────────────────────────────┐  │
│  │  containerd (optional, only with cri runtime)       │  │
│  │  Container execution, image pull, networking        │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                           │
└───────────────────────────────────────────────────────────┘

         ▲
         │  kubectl apply / kubectl get / CRDs
         │  (standard kubeconfig, no special client)
         │
      [ User / CI / GitOps ]
```

### Data Flow

1. **User** runs `kubectl apply -f deployment.yaml`.  This hits the **apiserver**
   over the standard HTTPS endpoint.
2. **apiserver** validates, admits, and persists the object to the kine/SQLite
   datastore.
3. **scheduler** watches for unbound Pods, scores the single nodelet Node, and
   writes the binding (`spec.nodeName`).
4. **nodelet** has a watch open on Pods with `fieldSelector=spec.nodeName=<self>`.
   It receives the new Pod event.
5. **nodelet** calls its pluggable runtime (`mock` or `cri`) to "run" the pod.
6. **nodelet** patches the Pod status to `Running` (or `Failed`, etc.) on the
   apiserver.
7. **kubectl get pods** shows the updated status.

### Component Responsibilities

| Component | Source | Role |
|-----------|--------|------|
| k3s server (--disable-agent) | Upstream Rancher k3s | Real apiserver + scheduler + controller-manager + kine/SQLite.  Full kubectl/CRD compatibility. |
| nodelet | `/workspace/not-k8s/crates/nodelet` (Rust) | Registers a Node object, maintains Lease heartbeat, watches & reconciles Pods, delegates to runtime. |
| containerd | System package (optional) | Container execution engine, used only when `NODELET_RUNTIME=cri`. |

## Why Replace the Node Side, Not the API Server

It is tempting to think "rewrite everything in Rust for maximum efficiency."
Here's why that's the wrong approach for the apiserver:

**The apiserver is a compatibility swamp.** It implements:
- OpenAPI v2/v3 schema serving (every `kubectl explain` and client-gen depends on this)
- RBAC authorization with dozens of built-in roles
- Admission webhooks (mutating + validating) — the entire ecosystem of operators relies on these
- Server-side apply (field ownership tracking)
- Watch multiplexing with bookmarks and resource versions
- CRD storage, validation, conversion webhooks
- Discovery endpoints (`/api`, `/apis`, `/openapi/v2`)
- Aggregated API servers

Reimplementing any of these breaks compatibility with the vast kubectl / Helm /
operator ecosystem.  And the apiserver is **not where the idle cost lives** —
it's a watch multiplexer that's relatively quiet when nothing is changing.

**The node side has a clean contract and is where the cost lives.**  The kubelet's
interface to the control plane is narrow:
- Register a Node object
- Maintain a Lease
- Watch Pods with `fieldSelector=spec.nodeName=<self>`
- Patch Pod status
- (Optional) Report Node status/conditions

This is a well-defined, stable API.  And it's where PLEG, cAdvisor, and the
per-component watch caches burn CPU at idle.  Replacing this narrow contract
with an efficient implementation is tractable and high-leverage.

## How nodelet Stays Cheap

### Event-Driven Reconciliation (No Polling)

Stock kubelet uses PLEG, which **relists all containers every 1 second** via CRI
`ListPodSandbox` + `ListContainers`, compares with the previous state, and
generates events.  This burns CPU constantly even with zero running containers.

nodelet uses the apiserver's **watch** mechanism: it maintains a single long-lived
HTTP watch on Pods bound to its node.  Reconciliation only happens when the
apiserver pushes an event.  At idle, this is a single open TCP connection with
zero CPU cost.

### CRI Event Stream Instead of PLEG

When using the `cri` runtime, nodelet subscribes to containerd's **event stream**
(`/containerd.services.events.v1.Events/Subscribe`) for container lifecycle
notifications.  State changes are pushed, not polled.

### On-Demand Stats Instead of cAdvisor

Stock kubelet embeds cAdvisor, which runs housekeeping loops every 10-15s per
container, scraping cgroup stats, filesystem usage, and network counters.  This
data feeds `/metrics/cadvisor`, summary API, and eviction decisions.

nodelet reads cgroup v2 stats and PSI (Pressure Stall Information) **on demand
only** — when a status update is due or when explicitly queried.  No background
housekeeping loops.

### Decoupled Heartbeat and Status Push

nodelet separates two concerns:
- **Lease heartbeat** (`NODELET_HEARTBEAT_SECS`, default 10s): A lightweight
  Lease object update that tells the control plane "I'm alive."  This is a
  tiny PUT (~200 bytes).
- **Node status push** (`NODELET_STATUS_SECS`, default 60s): A larger update
  with capacity, conditions, allocatable resources.  This is infrequent because
  the data rarely changes on a single device.

Stock kubelet conflates these, pushing full NodeStatus every 10s by default.

### Single Process, Single Watch Cache

Stock k3s runs: kubelet + kube-proxy + containerd + containerd-shim(s).  Each
process maintains its own informer/watch cache — duplicating in-memory
representations of the same API objects.

nodelet is **one process** with **one kube client** and **one watch cache**.
Memory usage scales with the number of pods bound to this node, not the number
of system components.

## Pluggable Runtime: mock vs cri

nodelet's runtime is a trait (Rust interface) with two implementations:

```
trait Runtime {
    async fn run_pod(&self, pod: &Pod) -> Result<PodStatus>;
    async fn stop_pod(&self, pod: &Pod) -> Result<()>;
    async fn pod_status(&self, pod: &Pod) -> Result<PodStatus>;
}
```

### mock Runtime (default)

- **No container engine needed.**
- Tracks pod state in memory.
- Immediately reports all containers as `Running`.
- Purpose: measure and demonstrate the control-loop overhead *without* any
  container-engine noise.  Isolates the cost of nodelet's API interactions.
- Build: `cargo build --release` (no feature flags).
- Use: `NODELET_RUNTIME=mock` (default).

### cri Runtime

- Connects to containerd via the CRI gRPC socket.
- Pulls images, creates sandboxes, starts containers — real workloads.
- Subscribes to containerd event stream for lifecycle updates.
- Build: `cargo build --release --features cri`.
- Use: `NODELET_RUNTIME=cri` with `NODELET_CRI_ENDPOINT=unix:///run/containerd/containerd.sock`.

### Why mock Exists

The mock runtime is not just a test stub.  It's a **measurement tool**.

When you run the full system with mock, you can measure the exact idle overhead
of the control plane + node agent with zero container-engine contribution.  This
isolates the architectural cost and proves (or disproves) the thesis that
event-driven reconciliation is fundamentally cheaper than polling.

Comparing `k3s (stock, idle)` vs `k3s (--disable-agent) + nodelet (mock, idle)`
gives you a clean A/B measurement of the node-agent architecture change.

## Configuration Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `KUBECONFIG` | Standard kube client resolution | Path to kubeconfig pointing at the local k3s apiserver |
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
| `NODELET_CPU_MANAGER_POLICY` | `none` | `none` or `static` (cri runtime only) — pins Guaranteed-QoS containers requesting a whole number of CPUs to exclusive cores |
| `NODELET_TOPOLOGY_MANAGER_POLICY` | `none` | `none`, `best-effort`, `restricted`, or `single-numa-node` (cri runtime only) — coordinates CPU Manager and device plugins by NUMA node |
| `RUST_LOG` | (none) | Tracing filter, e.g. `info`, `nodelet=debug,kube=warn` |

## Out of Scope

For everything kubelet itself is responsible for, the goal is now **100%
feature parity** (see [`docs/GAP_CLOSURE.md`](GAP_CLOSURE.md) for the live,
verified-against-upstream-docs checklist of done/partial/missing). The list
below is different in kind: these are either genuinely another
control-plane component's job (verified, not assumed — see GAP_CLOSURE.md's
scope-boundary section), or a deliberate platform choice unrelated to
kubelet parity.

- **High availability / multi-node scheduling.** This is a single-device system.
  The scheduler runs, but there's one node.
- **etcd quorum.** kine/SQLite is the datastore.  No Raft, no peer election.
- **Server-side apply.**  Client-side apply (`kubectl apply`) works via the
  stock apiserver.  SSA field management is supported by the apiserver itself;
  nodelet doesn't need to do anything special.
- ~~**kube-proxy / Service networking.**~~ Implemented: `nodelet` watches
  Services/Endpoints and programs ClusterIP/NodePort routing itself via
  nftables (`crates/nodelet/src/svc.rs`) — no separate kube-proxy process.
  With mock runtime there's still no real networking at all (nothing to
  route to). See the README's "Services (ClusterIP / NodePort)" section.
- **Mesh federation sync agent.**  Future work: devices federate to an upstream
  cluster over Tailscale/Netbird, syncing selected resources bidirectionally.
  This is a separate component, not part of the current architecture.
- **Custom scheduler / admission webhooks.**  The stock k3s scheduler and
  admission pipeline work as-is.
- **Windows / non-Linux.**  cgroup v2, `/proc`, and the CRI socket are
  Linux-specific.

## Roadmap

| Phase | What | Status |
|-------|------|--------|
| **1. Baseline measurement** | Measure stock k3s idle overhead (CPU, RSS) on target hardware.  This is the number to beat. | Tooling ready (`deploy/measure.sh`) |
| **2. nodelet mock runtime** | Implement node registration, Lease heartbeat, Pod watch, mock runtime.  Measure idle overhead and compare with baseline. | In progress (`crates/nodelet`) |
| **3. nodelet CRI runtime** | Add containerd integration via CRI gRPC.  Run real workloads.  Measure overhead delta vs. mock. | Planned |
| **4. Trim control plane** | Replace kine/SQLite with an in-memory store for volatile state.  Tune controller-manager intervals.  Reduce apiserver watch-cache memory. | Planned |
| **5. Mesh federation sync agent** | Bidirectional resource sync between edge device and upstream cluster over Tailscale/Netbird mesh. | Future |

## Limitations and Status

> **This is an early prototype.**

- **nodelet is under active development.**  The Rust binary in `crates/nodelet`
  is being built in parallel with these operational scripts.
- **Only the mock runtime is initially available.**  CRI support requires the
  `--features cri` build flag and a running containerd instance.
- **No automated tests yet** for the deployment scripts.  They are validated
  manually on target hardware.
- **Single-node only.**  While the architecture doesn't preclude multiple nodelet
  instances on different devices talking to the same apiserver, this is not
  tested or documented.
- **No graceful upgrade path.**  Upgrading k3s or nodelet currently means
  stopping, replacing binaries, and restarting.
- **Security hardening is incomplete.**  The kubeconfig is written world-readable
  for convenience; the nodelet runs as the current user; no mTLS between
  components beyond what k3s provides by default.
- **Resource eviction is not implemented.**  nodelet reports real
  MemoryPressure/DiskPressure conditions but does not yet act on them by
  evicting pods.  This is kubelet's job (not another component's) and is
  tracked as an open item in `docs/GAP_CLOSURE.md`, not a non-goal.
