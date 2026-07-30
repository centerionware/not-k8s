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
- ❌ Ephemeral containers (`kubectl debug`)
- ✅ **postStart / preStop lifecycle hooks** (`exec`/`httpGet`/`sleep`; not `tcpSocket`) — `run_lifecycle_hook()`. A failing `postStart` is logged, not (yet) turned into a container kill+restart like real kubelet does.
- ✅ **Termination grace period** — `terminationGracePeriodSeconds` now drives `preStop` + a per-container `StopContainer` timeout before `StopPodSandbox` (`graceful_stop_containers()`), instead of an untimed sandbox stop.
- ✅ **Container restart count** — real per-container counter (`restart_count_from`/`bump_restart_count_in`), threaded through `ContainerConfig.metadata.attempt` too (so restarted containers get distinct log files, not overwritten ones).
- ✅ **Exit-code-aware phase computation** — `restartPolicy: Never` now reports `Failed` (not `Succeeded`) when a container exited nonzero (`compute_phase()`'s new `any_failed` parameter).

### Resource management
- ✅ **Container resource requests/limits** — translated to CRI `LinuxContainerResources` (cpu shares/quota/period, memory limit; `linux_resources()`)
- ❌ QoS cgroup hierarchy (`--cgroups-per-qos`)
- ❌ cgroup driver detection/consistency (cgroupfs vs systemd) with the container runtime
- ❌ Node allocatable enforcement (`--enforce-node-allocatable`, system-reserved/kube-reserved cgroups)
- ❌ CPU Manager (`static` policy, exclusive core pinning) — advanced/optional in real kubelet
- ❌ Memory Manager — advanced/optional
- ❌ Topology Manager — advanced/optional
- ❌ Device plugins (GPU/FPGA/etc. hardware resources)
- ❌ In-place pod resource resize (newer, still-maturing upstream feature)

### Security context
- ✅ **`securityContext`** — `runAsUser`/`runAsGroup`, capabilities add/drop, `privileged`, `readOnlyRootFilesystem`, `allowPrivilegeEscalation`→`no_new_privs`, `supplementalGroups`, `seccompProfile` (`linux_security_context()`). Not yet: `runAsNonRoot` verification against the image's actual user, AppArmor profile, SELinux options.
- ❌ Pod-level `sysctls`
- ✅ **`fsGroup` volume ownership application** — recursive chown + setgid on every volume directory nodelet itself materializes (`apply_fs_group()`). Only reaches those (ConfigMap/Secret/emptyDir/downwardAPI/projected) — there's no real PV/hostPath for it to reach beyond that yet.
- ❌ RuntimeClass (gVisor/Kata/etc. runtime selection + pod overhead accounting)

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
- ❌ PersistentVolumeClaim / CSI (`NodeStageVolume`/`NodePublishVolume`) — no PVC support at all
- ❌ hostPath (explicitly unsupported today, logged and dropped)
- ❌ `emptyDir.sizeLimit` enforcement
- ❌ subPath `$(VAR)` expansion

### Node-pressure eviction
- ✅ MemoryPressure/DiskPressure *conditions* reflect real reads
- 🟡 **Eviction** — `eviction_loop()` now acts on real pressure: ranks eligible pods by QoS class (`eviction.rs`'s `qos_class()`/`pick_eviction_candidate()` — BestEffort before Burstable, Guaranteed and `system-*-critical` pods never evicted), evicts one per check. Simplified vs. real kubelet: no soft-threshold grace period (hard-style immediate action only), and ranking within a QoS class uses *requested* memory as a proxy, not live usage — there's no per-pod cgroup stats collector yet (the `/stats/summary` gap below).
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
- ❌ **`/stats/summary`** (and `/metrics/resource`, `/metrics/cadvisor`) — the API metrics-server scrapes for `kubectl top node/pod`. Still the biggest single remaining gap; would also make eviction ranking usage-based instead of request-based (see eviction.rs's own note on this).
- ❌ Client certificate authentication (bearer token only)

### Node shutdown
- ❌ Graceful node shutdown (systemd inhibitor lock, priority-ordered pod eviction on `shutdown -h now`)

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
- [ ] Everything else in the responsibility list above — biggest remaining
      single items: `/stats/summary` + real per-pod usage stats (would also
      make eviction ranking usage-based instead of request-based), PVC/CSI,
      ephemeral containers, RuntimeClass, graceful node shutdown, cgroup
      driver/QoS hierarchy/node allocatable enforcement, CPU/Memory/
      Topology managers, device plugins. Ask before starting the next round.
