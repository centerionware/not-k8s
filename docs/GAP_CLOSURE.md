# kubelet parity gap closure — working memory

Started 2026-07-30. Tracks closing the 3 real gaps found by reading the code
(not assumptions) against kubelet's core responsibilities. See
`ARCHITECTURE.md`'s "Out of Scope" section for what's *deliberately* not done
(HA, etcd quorum, SSA, eviction) — this doc is only about things that were
genuinely missing, not intentional non-goals.

## Tension to keep in mind

nodelet's whole point is shedding kubelet's idle-polling cost (no PLEG, no
cAdvisor housekeeping — see `runtime/mod.rs` module doc). Probes and pressure
metrics are inherently periodic checks. Resolution: cost is opt-in and scoped
—

- Probes only run for pods that actually declare a `livenessProbe` /
  `readinessProbe` / `startupProbe`. Zero probes defined anywhere on the node
  → zero added polling, idle cost is unchanged.
- Pressure metrics piggyback on the existing infrequent status-push interval
  (default 60s) — no new timer.
- GC runs on its own coarse interval (default 5 min), not per-second.

## Gaps (found by grep/read, not docs)

1. **Health probes** — no liveness/readiness/startup probe execution existed
   anywhere. Container restarts only reacted to CRI-observed exits, never to
   a failed probe.
2. **Live pressure metrics** — `MemoryPressure`/`DiskPressure` node
   conditions were hardcoded `"False"` in `node.rs::conditions()`, never
   measured.
3. **Garbage collection** — no image GC, no orphaned sandbox/container
   cleanup (e.g. pod deleted from the apiserver while nodelet was down).

## Design decisions

### Probes (`src/probes.rs`)
- Pure state machine (`ProbeTracker`) for success/failure-threshold counting
  — unit tested without touching the network.
- HTTP probe: hand-rolled minimal HTTP/1.1 GET over `TcpStream` (status line
  only) — avoids pulling in a full HTTP client dependency.
- TCP probe: plain `TcpStream::connect` with a timeout.
- Exec probe: new `PodRuntime::exec()` trait method. CRI implements via CRI
  `ExecSync`; mock always succeeds.
- Restart-on-liveness-failure: new `PodRuntime::restart_container()` trait
  method — CRI impl removes just that container (`RemoveContainerRequest`);
  reuses the existing `ensure_container` "no existing container → recreate"
  path on the next `ensure_pod()` call, so no new recreate logic needed.
- Readiness feeds back into `pods.rs::build_pod_status()` — per-container
  `ready`, and pod-level `Ready`/`ContainersReady` conditions. The stock
  apiserver's own EndpointSlice controller reacts to `Ready` automatically —
  nodelet does not need to touch Endpoints itself.
- One supervisor task per pod-with-probes, spawned from `PodController`,
  cancelled on teardown.

### Pressure metrics (`src/metrics.rs`)
- Memory: `/proc/meminfo` `MemAvailable`. Pressure if available bytes <
  threshold (default 100Mi, `NODELET_MEMORY_PRESSURE_THRESHOLD_BYTES`).
- Disk: `statvfs(2)` via a small `libc` FFI call (already an indirect
  dependency, cheap to make direct) against a configurable path (default
  `/var/lib/nodelet`, `NODELET_DISK_PATH`). Pressure if available % <
  threshold (default 10%, `NODELET_DISK_PRESSURE_PERCENT`).
- Out of scope (documented, not silently dropped): a full
  metrics-server-compatible `/stats/summary` HTTP endpoint. That's a much
  larger surface (serving kubelet's stats API, cAdvisor-shaped per-pod
  cpu/mem) and wasn't what was hardcoded/fake before — the concrete bug was
  the two conditions always reporting healthy.

### Garbage collection (`src/gc.rs`)
- Orphan sandbox/container cleanup: periodic (default 300s,
  `NODELET_GC_INTERVAL_SECS`) diff of CRI-known `nodelet.dev`-labelled
  sandboxes against Pods currently bound to this node in the apiserver;
  anything CRI has that the apiserver doesn't gets removed.
- Image GC: same interval, removes images not referenced by any current
  container/sandbox on the node.
- Both pure decision functions (`orphaned_sandboxes`, `images_to_gc`) are
  unit tested independent of a real CRI socket.

## Progress

- [x] This doc
- [x] Probes (`src/probes.rs`, wired into `pods.rs`)
- [x] Pressure metrics (`src/metrics.rs`, wired into `node.rs`)
- [x] GC (`src/gc.rs`, wired into `runtime/cri.rs` + `main.rs`'s `gc_loop`)
- [x] Full test suite green: 164 passed, 0 failed (`cargo test -p nodelet --features cri`); default (mock-only) build also compiles clean

## What's still explicitly out of scope (not silently dropped)

- A metrics-server-compatible `/stats/summary` HTTP endpoint (per-pod cpu/mem
  usage for `kubectl top`). The concrete bug fixed here was two node
  conditions always reporting healthy — a full cAdvisor-shaped stats API is
  a much larger, separate surface.
- HTTPS probe TLS handshake — `httpGet` probes with `scheme: HTTPS` fall
  back to a bare TCP connect check (proves the port is open, not that the
  handshake/response succeeds). Noted in `probes.rs`.
- kubelet's fully independent per-probe-type timers — nodelet ticks
  liveness+readiness together at the faster of the two configured periods
  for a given container. Acceptable simplification for single-node edge use.
- Real per-container `restartCount` tracking (pre-existing known gap, not
  touched by this pass — still pinned at 0, see `pods_tests/build_pod_status.rs`).
