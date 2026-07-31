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
this performance advantage** — see [`docs/GAP_CLOSURE.md`](docs/GAP_CLOSURE.md)
for the live, verified checklist of what's done vs. still missing.

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
through too, closed alongside this round's cgroup work. Still missing:
PVC/CSI, CPU/Memory/Topology managers, device plugins — full list in
`docs/GAP_CLOSURE.md`.

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
| `RUST_LOG` | `info` | Tracing filter, e.g. `nodelet=debug`. |

---

## License

MIT OR Apache-2.0.
