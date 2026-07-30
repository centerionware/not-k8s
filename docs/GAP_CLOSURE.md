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

## Full kubelet responsibility list vs. current nodelet state

Legend: ✅ done · 🟡 partial · ❌ missing

### Pod & container lifecycle
- ✅ Pod sandbox + container create/start/stop/remove via CRI
- ✅ Restart-on-exit honoring `restartPolicy`
- ✅ **Init containers** — run to completion, in order, before app containers (`ensure_init_containers`)
- ❌ Ephemeral containers (`kubectl debug`)
- ❌ postStart / preStop lifecycle hooks
- ❌ Termination grace period (`terminationGracePeriodSeconds`) — teardown calls `StopPodSandboxRequest` with no grace/SIGTERM-then-SIGKILL sequencing
- 🟡 Container restart count — always reported as `0` (known pre-existing gap, pinned by test)
- 🟡 Exit-code-aware phase computation — `restartPolicy: OnFailure` doesn't yet distinguish "exited 0" from "exited nonzero" (documented limitation in `cri.rs`)

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
- ❌ `fsGroup` volume ownership application
- ❌ RuntimeClass (gVisor/Kata/etc. runtime selection + pod overhead accounting)

### Networking
- ✅ **DNS config** — `dnsPolicy`/`dnsConfig` → CRI `PodSandboxConfig.dns_config` (`dns_config_for()`), via new `NODELET_CLUSTER_DNS`/`NODELET_CLUSTER_DOMAIN`
- ❌ `hostAliases` (`/etc/hosts` entries)
- ✅ Service/ClusterIP/NodePort routing (nftables — pre-existing, kube-proxy's job but already reimplemented here)

### Images
- ✅ **Private registry auth** — `imagePullSecrets` (`kubernetes.io/dockerconfigjson`) → CRI `AuthConfig` (`resolve_pull_auth()`). Not yet: legacy `kubernetes.io/dockercfg`, ServiceAccount-linked pull secrets, credential-provider exec plugins.
- 🟡 Image garbage collection — unreferenced-image sweep exists (this session) but not the real kubelet policy (disk-pressure-triggered high/low watermark GC, `--image-gc-high-threshold`/`--image-gc-low-threshold`)
- ❌ Container log rotation (`--container-log-max-size`/`--container-log-max-files`)

### Volumes
- ✅ ConfigMap / Secret / emptyDir (materialized to host paths)
- ❌ Projected volumes (the common `kube-api-access-*` service account token volume — currently skipped with a warning)
- ❌ downwardAPI volumes (downward API *env vars* work; the volume form doesn't)
- ❌ PersistentVolumeClaim / CSI (`NodeStageVolume`/`NodePublishVolume`) — no PVC support at all
- ❌ hostPath (explicitly unsupported today, logged and dropped)
- ❌ `emptyDir.sizeLimit` enforcement
- ❌ subPath `$(VAR)` expansion

### Node-pressure eviction
- ✅ MemoryPressure/DiskPressure *conditions* now reflect real reads (this session)
- ❌ **Actual eviction** — nodelet reports pressure but never acts on it: no soft/hard threshold config, no pod ranking by QoS class + usage-over-request, no pod termination to reclaim resources. `ARCHITECTURE.md` already flagged this as a known gap; it's now explicitly in scope.
- ❌ PID pressure — condition is hardcoded `False`; no real `/proc` PID accounting at all (unlike memory/disk, which are now real)

### Static pods & mirror pods
- ❌ Static pod manifest directory watching (`staticPodPath`)
- ❌ Mirror pod creation/reconciliation in the apiserver for static pods

### kubelet HTTP(S) server (entirely absent — nodelet has no listening server at all)
- ❌ **Streaming exec/attach/port-forward** — `kubectl exec`/`kubectl attach`/`kubectl port-forward` have no server to talk to
- ❌ **`kubectl logs`** — depends on the same server serving container logs
- ❌ TLS serving certificate (cert-dir, self-signed or CSR-issued) + client cert / bearer token authentication + webhook authorization for that server
- ❌ **`/stats/summary`** (and `/metrics/resource`, `/metrics/cadvisor`) — the API metrics-server scrapes for `kubectl top node/pod`. Explicitly called out as future work in the prior pass; now in scope.

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
- [ ] Everything else in the responsibility list above — not yet sequenced;
      ask before starting the next round given the remaining scope
      (streaming exec/logs server, eviction manager, static pods, CSI, ...)
