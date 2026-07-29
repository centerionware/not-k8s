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
  src/runtime/mock.rs  In-memory runtime: reports pods Running, zero engine overhead.
  src/runtime/cri.rs   Real containerd/CRI runtime (feature `cri`).
  proto/cri.proto      Upstream CRI v1 protobuf (vendored).
deploy/                Control-plane setup, launcher, measurement, demo manifests.
docs/ARCHITECTURE.md   Design, trade-offs, roadmap.
```

## Status

Early prototype. The control loop, node registration, Lease heartbeat, and both
runtimes compile and run. The `mock` runtime is fully exercisable today. The `cri`
runtime is **validated end-to-end against real containerd** (1.6.20): it creates a
sandbox, pulls an image, creates and starts a container under runc, reports status,
is idempotent, and tears down cleanly — see [Validate against containerd](#validate-against-containerd).
Not production-ready. No HA, no multi-node scheduling — by design (see *Out of scope*
in the architecture doc).

---

## Build

Requires a Rust toolchain (stable). The default build needs **no** extra system
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

`deploy/bootstrap-test.sh` is a single, self-contained script that installs
and smoke-tests the *entire* stack on any Linux box, regardless of distro or
CPU architecture — one command, nothing to install by hand first. It
re-execs itself under `sudo` automatically (root is needed to install system
packages and the k3s service), detects your package manager and arch, gets a
C toolchain and a Rust toolchain new enough to build this workspace (falling
back through official prebuilt releases, then a static cross toolchain, then
building gcc from source if truly nothing else is available), builds
`nodelet`, installs and starts the stripped k3s control plane, starts the
agent, and applies the demo pod. With `--with-cri` it also installs
containerd + runc (package manager -> official prebuilt -> built from source
via a from-scratch Go toolchain bootstrap) and starts containerd itself.

```bash
./deploy/bootstrap-test.sh                     # installs everything, mock runtime
./deploy/bootstrap-test.sh --with-cri          # + containerd/runc, real containers
./deploy/bootstrap-test.sh --skip-control-plane  # bring your own KUBECONFIG, no root needed
./deploy/bootstrap-test.sh --cleanup           # tear down everything it started
```

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
  network config in `/etc/cni/net.d`, which `bootstrap-test.sh` installs.
- Per-node pod subnet allocation needs no new code on the control-plane side:
  k3s's controller-manager runs `--allocate-node-cidrs` regardless of
  `--flannel-backend`, which is exactly what lets `--flannel-backend=none`
  (already the flag `setup-control-plane.sh` uses) be paired with any
  self-managed CNI — flannel included.
- `bootstrap-test.sh` installs the standard CNI plugins
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

What's still missing: **Services (ClusterIP) need kube-proxy**, which is part
of the kubelet/agent this project deliberately doesn't run. Direct pod-to-pod
traffic by pod IP works today; Service routing is on the roadmap.

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
side.

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
| `RUST_LOG` | `info` | Tracing filter, e.g. `nodelet=debug`. |

---

## License

MIT OR Apache-2.0.
