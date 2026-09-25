# nodemigrate implementation and integration status

Last updated: 2026-09-25

This is the living implementation status record for the full scope in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). Capability marks describe code
that exists; verification marks describe evidence from a run. A passing
compile or unit test does not mark a real migration path as verified.

## Latest runtime defect

The branch-runtime migration at `3ef3ddf1e544a4ba7eda328c1b841ccd51aee9e1`
confirmed that nodeapiserver ignored controller impersonation headers and
authorized ReplicaSet creates as `system:kube-controller-manager`. The server
now performs RBAC checks for each impersonated ServiceAccount and group before
using that identity for the request. Focused `nodeapiserver,nodemigrate`
quick-check passed at SHA `d4847449a84a5014515f31b3d9d522e455910cf7` in
[run 36080595243](https://github.com/centerionware/not-k8s/actions/runs/36080595243).
The branch-runtime migration rerun is [run 36080871524](https://github.com/centerionware/not-k8s/actions/runs/36080871524); its migration result is still pending.

## Migration coverage

| Scenario or behavior | Implementation | Verification |
| --- | --- | --- |
| Detect local K3s service, Kine/etcd mode, config, and network settings | Implemented; external CNI config and binary directories are read from the active containerd config, with existing `/etc/cni/net.d` detection retained when that is the configured source | K3s external-CNI path fixtures passed in [run 36026420825](https://github.com/centerionware/not-k8s/actions/runs/36026420825) |
| Detect K3s agent and upstream Kubernetes worker roles | Implemented in inventory, including role-specific services and external CNI detection | Focused role fixtures passed in [run 35959159615](https://github.com/centerionware/not-k8s/actions/runs/35959159615). |
| Replace a worker against an existing nodestore cluster | Worker path avoids cluster-wide object import, requires a joined destination, explicitly guards same-name Node removal, uses a UID-preconditioned delete, waits for fresh Ready registration, preserves labels/annotations/taints/unschedulable state, and snapshots local hostPath/local PV data from the destination PV inventory | Existing role and affinity checks passed in [run 35960475945](https://github.com/centerionware/not-k8s/actions/runs/35960475945); scheduling-state extraction passed in [run 35967908377](https://github.com/centerionware/not-k8s/actions/runs/35967908377). Node annotation preservation and UID-preconditioned deletion were added after those runs; targeted CI is pending. Real worker cutover, data restoration, rollback, and uninstall behavior remain unverified. |
| Replace a migrating control-plane Node object | Forward and reverse migration require `NODEMIGRATE_REPLACE_NODE=true` when the destination already has the local node name. For a reverse control-plane cutover whose retained API is unavailable before activation, the flag is also required to force a fresh registration check. The stale Node is deleted with a UID precondition before waiting for fresh Ready, then its labels, annotations, taints, and unschedulable setting are restored. If no destination Node existed before activation, state captured from nodestore remains authoritative even if the newly activated kubelet registers the same name before deletion. A staged reverse cutover returns before API readiness, so its coordinator must verify fresh registration after quorum returns. | State preservation and proactive confirmation passed in [run 35968225182](https://github.com/centerionware/not-k8s/actions/runs/35968225182). Node annotation preservation, UID-preconditioned deletion, and the round-trip fixture were added after those runs; targeted CI is pending. No live registration or replacement-member runtime evidence. |
| Return a not-k8s worker to a retained K3s/upstream worker | Worker inventory recognizes nodelet-only hosts; reverse path snapshots local PV data, stops nodebootstrap worker services, starts the retained target agent, optionally replaces a stale same-name Node, and waits for fresh Ready without re-importing cluster resources | Role fixtures and nodemigrate checks passed in [run 35960872299](https://github.com/centerionware/not-k8s/actions/runs/35960872299). No live reverse worker cutover or multi-node round trip recorded. |
| Stop upstream control-plane static pods before cutover | After kubelet stops, remove file-managed kube-system pod sandboxes via CRI while retaining source manifests; endpoint is detected from kubelet instance config or overridden with `NODEMIGRATE_CRI_ENDPOINT` | CRI inventory and endpoint fixtures passed in [run 35960125413](https://github.com/centerionware/not-k8s/actions/runs/35960125413). No live kubeadm cutover evidence. |
| Detect an upstream control plane and read CIDRs and cluster domain | Implemented | Focused fixture passes after the rooted manifest path correction |
| Identify Cilium and other known CNIs from the active host CNI configuration | Implemented for recognized CNI config names and plugin types | Focused fixture tests passed in [run 35955823452](https://github.com/centerionware/not-k8s/actions/runs/35955823452) |
| Export listable API resource pages, including CRDs and add-ons | Implemented with source UIDs stored in a protected manifest beside exported objects, leaving user annotations unchanged. Live NodeMetrics/PodMetrics and controller-regenerated/static mirror Pods are omitted; standalone Pods and ReplicaSet/ControllerRevision rollout history are preserved. | Focused exporter tests and round-trip fixtures for standalone Pods and StatefulSet history are in the current branch; targeted CI is pending. Real API parity evidence is pending. |
| Import objects when source and destination serve different versions | Implemented: if the exact source `apiVersion` is absent, discovery selects a destination version with the same API group and kind, submits the object through that version, and logs the conversion. The destination API server remains responsible for conversion and schema validation. | Focused nodemigrate crate checks passed at SHA `954f1be3d12fb7f0334db8dff6efae28ca0e4250` in [run 36068373192](https://github.com/centerionware/not-k8s/actions/runs/36068373192). Run 36068417485 confirms ClusterTrustBundle applied through v1beta1; import then failed on a CSR because the v0.8.0 target has the unreleased ExtraValue codec bug. |
| Restore the source after a failed forward cutover | On bootstrap, target-readiness, or object-import failure, stop partially installed nodestore services before restoring the source's previous service state; keep the protected API export and report whether rollback succeeded. The integration lane checks source service/API readiness and export retention when migration exits unsuccessfully. | Nodemigrate crate tests passed at SHA `f9e4b31139a30fb7a453161e4dc2295983fcef8f` ([run 36071161354](https://github.com/centerionware/not-k8s/actions/runs/36071161354)). The release-backed CSR import failure exercised rollback in [run 36071174265](https://github.com/centerionware/not-k8s/actions/runs/36071174265), attempt 2: kubelet and source API recovered, and the protected export remained. The overall lane correctly stayed failed because v0.8.0 still has the codec defect. |
| Preserve persistent volume data and provider behavior | The current code backs up static hostPath/local PV paths, filtering by source Node labels and required PV node affinity when available; if labels cannot be read, it warns and keeps every safe local candidate. CSI-backed data remains with its provider, and the current dynamic CSI fixture verifies data only in a same-host round trip. The full goal requires preserving each source provisioner's configuration, credentials, attachment behavior, and payload access; no provisioner class is excluded by assumption. | Affinity fixtures passed in [run 35965492161](https://github.com/centerionware/not-k8s/actions/runs/35965492161); no round-trip evidence for static or dynamic data and no multi-node provider/attachment evidence |
| K3s → nodestore with Flannel | Implemented | No real migration run recorded |
| K3s → nodestore with external CNI such as Cilium | Implemented as external-CNI bootstrap and API-resource transfer. The detected CNI directories are passed to nodebootstrap and containerd is configured to use them. If explicit K3s uninstall would remove CNI directories inside the K3s data directory, nodemigrate snapshots their contents and restores them at the configured paths before reconciling nodebootstrap. | Focused detection and uninstall-snapshot tests passed in [run 36026420825](https://github.com/centerionware/not-k8s/actions/runs/36026420825); containerd config tests passed in [run 36025823559](https://github.com/centerionware/not-k8s/actions/runs/36025823559). Real K3s uninstall and workload networking remain unverified. |
| Upstream Kubernetes → nodestore | Implemented for detected local control planes | No real migration run recorded |
| Join an existing nodestore control plane using nodebootstrap environment settings | Implemented through nodebootstrap join configuration, then a worker bootstrap registers the replacement node | Focused command tests passed in [run 35955823452](https://github.com/centerionware/not-k8s/actions/runs/35955823452); no real replacement-node run recorded |
| Migrate later source control planes and workers without re-importing cluster-wide objects | `skip-api-import=true` lets later control planes join the existing nodestore cluster without re-applying the cluster export; workers already avoid cluster-wide imports. Both node roles can reuse the protected source export after source API quorum is lost. | Targeted nodemigrate CI pending; first-control-plane import and multi-node runtime ordering remain unverified |
| Continue ordered forward migration after source API quorum is lost | Protected export format v2 includes every source Node's UID, labels, annotations, taints, and unschedulable state. A later control-plane or worker can use `source-export=<private-copy>` with `skip-api-import=true`, avoiding source API reads while preserving that node's state and selecting its local hostPath/local PV paths. Per-node backup directories avoid overwriting backups in a reused export copy. | Manifest round-trip and separate node-affined hostPath backup/restore tests passed in [run 36029431383](https://github.com/centerionware/not-k8s/actions/runs/36029431383); source-quorum-loss and live volume snapshots remain unverified |
| Return a multi-control-plane cluster when either datastore needs peers to reach quorum | `stage-target=true` exports nodestore state, disables the local nodestore stack, starts the retained target control plane, and returns before API readiness. Once target quorum returns, `import-export=/protected/export/path` loads the protected manifest and imports the saved API objects without needing the stopped source API. Later CPs can use `skip-api-export=true` after cluster state is imported; it requires a ready destination API and backs up local PV data from that API. All options require CP roles at both ends. | Request parsing, export-manifest recovery, and source annotation preservation passed in [run 35976429487](https://github.com/centerionware/not-k8s/actions/runs/35976429487). No three-CP runtime evidence; sequencing remains operator-ordered, not automatically coordinated. |
| Replace an existing nodestore member | Implemented: wait for the replacement Kubernetes node to become Ready, require the learner to be active and caught up through the leader log tail, promote it, verify voter status, then remove the explicitly identified old member. A failed promotion leaves the old member in place; the membership operation is retryable. | Targeted nodestore/nodebootstrap/nodemigrate checks passed in [run 35957212012](https://github.com/centerionware/not-k8s/actions/runs/35957212012), [run 35957207376](https://github.com/centerionware/not-k8s/actions/runs/35957207376), and [run 35957207356](https://github.com/centerionware/not-k8s/actions/runs/35957207356); no real joined replacement run recorded |
| Nodestore → retained K3s | Implemented; target must already be installed locally | No real migration run recorded |
| Nodestore → retained upstream Kubernetes | Implemented; target must already be installed locally | No real migration run recorded |
| Keep source installed and disabled by default | Implemented | No real service-manager cutover run recorded |
| Explicit source uninstall for K3s, kubeadm, or nodebootstrap | Implemented | Not yet exercised; use only in a disposable integration VM |

## Dedicated integration workflow

See the [CI status](NODEMIGRATE_CI_STATUS.md) for test policy and run history.
`.github/workflows/nodemigrate-integration.yml` is a manual, isolated runner
workflow. It builds the combined runtime and standalone migration binary, then
runs `.github/scripts/nodemigrate-integration.sh` once for each starting
distribution:

1. Install K3s or kubeadm Kubernetes as the initial source. K3s starts with
   Flannel disabled; both paths install Cilium as the external CNI.
2. Install the repository's real hostPath CSI test driver, cert-manager,
   Traefik, an nginx workload, a static hostPath PV/PVC, and a CSI-backed
   dynamic PVC. A pod writes unique data to both volumes.
3. Verify the source node, add-ons, certificate, ingress route, bound claims,
   volume data, and Cilium DaemonSet/CRD state.
4. Migrate source → nodestore and repeat the same checks against the
   nodebootstrap kubeconfig.
5. Migrate nodestore → the retained source distribution and repeat the checks
   again.
6. Exercise a separate existing-cluster replacement lane: join as a learner,
   wait for the replacement Kubernetes node to become Ready and the learner to
   catch up, promote it, then retire the configured old member ID. The current
   integration script does not yet implement this lane.

The workflow is manual. The user authorized the dedicated migration runtime
workflow while excluding the standard e2e and build gates. Run
[36058570338](https://github.com/centerionware/not-k8s/actions/runs/36058570338)
used the `v0.8.0` runtime: both fixture setups and target bootstraps passed,
then import failed because the target did not expose the source CRD APIs after
a 60-second wait. The unattended run printed the five-emoji high-risk warning
to its log. Neither lane completed a round trip. See the [CI status](NODEMIGRATE_CI_STATUS.md)
for per-lane counts and failure history.

## Merge-gate status

The required pre-merge runtime evidence is still missing for both gates:
single-node K3s+Cilium → not-k8s → K3s with no differences from the initial
full migration-state checkpoint, and upstream Kubernetes with three
control-plane nodes plus two workers → not-k8s → upstream Kubernetes. Both
require successful workload,
add-on, ingress, certificate, persistent-data, and Cilium checks at every
stage; the five-node case also requires the existing-cluster join/replacement
path. The existing single-host lane captures a fingerprint for every listable
API object handled by the export sanitizer, compares source→nodestore objects,
and compares full initial/returned semantic snapshots. It also checks node
names/roles/readiness, workload/add-on/ingress specs, PV/PVC bindings, Cilium,
required CRD state, and certificate-secret content by digest. The latest
single-host run is diagnostic only and failed during API import; it does not
pass the K3s merge gate. The Docker-only preflight now verifies five distinct
systemd containers, network/mount namespaces, CRI and BPF support, persistent
volumes, peer reachability, and node stop/restart isolation in [run
36077448685](https://github.com/centerionware/not-k8s/actions/runs/36077448685).
It does not establish Kubernetes control-plane behavior, Cilium datapath, or
five-node migration parity. QEMU
or another suitable isolation method can host those nodes on one CI node;
Docker is acceptable only if the environment fully simulates the behaviors
under test. No runtime merge gate has passed. The new skip-import path
addresses repeated cluster-wide API application on later forward control-plane
joins. The staged reverse path still needs an orchestrator to sequence quorum
recovery and verify freshness for nodes whose staged invocation returns before
the target API is ready.

The expanded object inventory has only passed static checks and has not run
against either source distribution. It covers API objects handled by the
export sanitizer; status and the sanitizer's transient-kind exclusions are
regenerated rather than compared. Runtime evidence is still required to
establish no differences across the full migrated state.

## Verification history

| Date | SHA | Check | Result | Evidence |
| --- | --- | --- | --- | --- |
| 2026-09-24 | `daac9ad05285e6ff749d4c51a18f9b71c8fb7dee` | Quick-check `nodebootstrap,nodemigrate` | Passed before bidirectional changes; does not validate reverse migration | [Run 35949477611](https://github.com/centerionware/not-k8s/actions/runs/35949477611) |
| 2026-09-24 | `55704b1c202c248ab633aac8c74877c46499e59f` | nodemigrate crate checks and targeted quick-check | Failed to compile; fixes are in the current worktree and follow-up runs are pending | [Crate run 35952766075](https://github.com/centerionware/not-k8s/actions/runs/35952766075), [quick-check run 35952782893](https://github.com/centerionware/not-k8s/actions/runs/35952782893) |
| 2026-09-24 | `4e932917080e21412c79625fb2b79d08f5e62c0f` | nodemigrate crate tests | Compiled; 9 passed, upstream network detection fixture failed due to doubled-root manifest path; correction pending | [Run 35954374767](https://github.com/centerionware/not-k8s/actions/runs/35954374767) |
| 2026-09-24 | `4ef3585eaea6beae51ffd1116134435b0edc305e` | nodemigrate crate tests | Passed after correcting rooted manifest path | [Run 35954606805](https://github.com/centerionware/not-k8s/actions/runs/35954606805) |
| 2026-09-24 | `0fe454dad5b3fb194b72a48bc657ed0512a7f29a` | Nodemigrate crate tests | Passed; covers skip-import parsing and rejection unless a control-plane joins an existing nodestore cluster. Does not verify a live multi-control-plane migration. | [Run 35962922792](https://github.com/centerionware/not-k8s/actions/runs/35962922792) |
| 2026-09-24 | `0fe454dad5b3fb194b72a48bc657ed0512a7f29a` | PR shell validation and commit convention | Passed | [Shell run 35962922860](https://github.com/centerionware/not-k8s/actions/runs/35962922860), [commit run 35962919894](https://github.com/centerionware/not-k8s/actions/runs/35962919894) |
| 2026-09-24 | `fc161b8c4262cdeb251abf6bbf2090e180d56c55` | K3s+Cilium integration round trip | Failed before migration: Cilium rollout passed, then the hostPath CSI pod could not start because `/opt/cni/bin/bridge` was missing. | [Run 36034461348](https://github.com/centerionware/not-k8s/actions/runs/36034461348) |
| 2026-09-24 | `fc161b8c4262cdeb251abf6bbf2090e180d56c55` | Upstream Kubernetes+Cilium integration round trip | Failed before migration: Cilium config init and operator crashed, then rollout timed out. | [Run 36034461348](https://github.com/centerionware/not-k8s/actions/runs/36034461348) |
| 2026-09-24 | `c651731652298f73540ba71a83b3c4cf6972bf64` | K3s+Cilium integration round trip | Failed before migration: runtime selected stale Podman bridge CNI config without the bridge and loopback plugins; the CSI hostPath plugin directory was absent. | [Run 36037082232](https://github.com/centerionware/not-k8s/actions/runs/36037082232) |
| 2026-09-24 | `c651731652298f73540ba71a83b3c4cf6972bf64` | Upstream Kubernetes+Cilium integration round trip | Failed before migration: Cilium's API endpoint `127.0.0.1` did not match kubeadm's API certificate SANs. | [Run 36037082232](https://github.com/centerionware/not-k8s/actions/runs/36037082232) |
| 2026-09-24 | `0655d0aacb6e7d90ee221061e7172842d63fcc99` | K3s+Cilium integration round trip | Failed before K3s installation: the installed CNI plugin package did not use the `/usr/lib/cni` path assumed by the fixture. | [Run 36039647520](https://github.com/centerionware/not-k8s/actions/runs/36039647520) |
| 2026-09-24 | `0655d0aacb6e7d90ee221061e7172842d63fcc99` | Upstream Kubernetes+Cilium integration round trip | Failed before kubeadm init because the fixture could not resolve the installed CNI plugin directory. | [Run 36039647520](https://github.com/centerionware/not-k8s/actions/runs/36039647520) |
| 2026-09-24 | `458c52c6247dfab8479cb18f33ae04a3db5b8c8d` | K3s+Cilium integration round trip | Cilium and hostPath CSI pods became available, but PVC provisioning failed because the CSI driver was not registered with the active kubelet and had no topology keys. Migration was not invoked. | [Run 36040401537](https://github.com/centerionware/not-k8s/actions/runs/36040401537) |
| 2026-09-24 | `458c52c6247dfab8479cb18f33ae04a3db5b8c8d` | Upstream Kubernetes+Cilium integration round trip | Failed before kubeadm init because the common setup installed `containernetworking-plugins`, which conflicts with kubelet's `kubernetes-cni` package. Migration was not invoked. | [Run 36040401537](https://github.com/centerionware/not-k8s/actions/runs/36040401537) |
| Worktree after `458c52c6` | — | Migration fixture follow-up | CNI packages are selected per source distribution, hostPath CSI setup now receives `/var/lib/kubelet` or `/var/lib/nodelet` for each active stage, and the Docker cgroup bind mount drops the invalid `rw=true` field. Runtime validation pending. | — |
| 2026-09-24 | `ef04b0e8c7e59b6f65bb41564139325d7026fdb9` | K3s+Cilium integration round trip | Cilium and hostPath CSI readiness passed, then the full archived e2e setup failed because it required nodelet DRA registration on native K3s. No migration was invoked. | [Run 36042056929](https://github.com/centerionware/not-k8s/actions/runs/36042056929) |
| 2026-09-24 | `ef04b0e8c7e59b6f65bb41564139325d7026fdb9` | Upstream Kubernetes+Cilium integration round trip | Cilium and hostPath CSI readiness passed, then the full archived e2e setup failed because it required nodelet DRA registration on native kubelet. No migration was invoked. | [Run 36042056929](https://github.com/centerionware/not-k8s/actions/runs/36042056929) |
| Worktree after `ef04b0e8` | — | Migration fixture follow-up | The helper now stops after the CSI setup section, K3s does not self-link package-installed CNI plugins, and the Docker probe prints container state/logs when systemd fails to start. Runtime validation pending. | — |
| 2026-09-24 | `580ae951144e0f2aa753a06e93e7a3f10264da75` | K3s+Cilium integration round trip | Every source-stage assertion passed, including Cilium, hostPath CSI/PVC data, cert-manager certificate, nginx, and Traefik routing. The first nodemigrate command printed the unattended risk warning then panicked before transfer because rustls had no selected CryptoProvider. | [Run 36043310369](https://github.com/centerionware/not-k8s/actions/runs/36043310369) |
| 2026-09-24 | `580ae951144e0f2aa753a06e93e7a3f10264da75` | Upstream Kubernetes+Cilium integration round trip | Kubeadm and source CSI setup passed; source workload setup timed out in Helm's Traefik `--wait` even though the Traefik Pod was Running. nodemigrate was not invoked. | [Run 36043310369](https://github.com/centerionware/not-k8s/actions/runs/36043310369) |
| Worktree after `580ae951` | — | Runtime follow-up | Installs the rustls ring CryptoProvider at utility startup and verifies Traefik with `kubectl rollout status` instead of Helm's broad wait. The Docker preflight enables systemd console logs while retaining private cgroup namespaces after exit 255. Runtime retest pending. | — |
| 2026-09-24 | `cefadddd0fa51dfc1a10d535160f1fc64a316b75` | K3s+Cilium integration round trip | Full source-stage checks passed, including all workloads and persistent data. Nodemigrate printed the unattended risk warning, then Tower panicked because no Tokio runtime was entered during Kubernetes client construction. No transfer completed. | [Run 36045632585](https://github.com/centerionware/not-k8s/actions/runs/36045632585) |
| 2026-09-24 | `cefadddd0fa51dfc1a10d535160f1fc64a316b75` | Upstream Kubernetes+Cilium integration round trip | Full source-stage checks passed, including the Traefik deployment and ingress route. Nodemigrate hit the same Tower/Tokio panic before transfer. | [Run 36045632585](https://github.com/centerionware/not-k8s/actions/runs/36045632585) |
| Worktree after `cefadddd` | — | Runtime follow-up | Enters the Tokio runtime while constructing the Kubernetes client and adds a focused unit regression test. Docker entrypoint prints its systemd path and enables debug console output; runtime retest pending. | — |
| 2026-09-24 | `1c42edc60fe45a5af654dd3a3f6d055047eacaff` | K3s+Cilium migration | The source-stage workload/data checks and unattended warning passed. Target bootstrap nevertheless enabled flanneld and failed to acquire a subnet. The CNI detector was taking K3s's implicit Flannel default before checking active Cilium. | [Run 36046899524](https://github.com/centerionware/not-k8s/actions/runs/36046899524) |
| 2026-09-24 | `1c42edc60fe45a5af654dd3a3f6d055047eacaff` | Upstream Kubernetes+Cilium migration | Source-stage checks and unattended warning passed. Target nodeapiserver failed readiness because 6443 was already bound; nodestore logs also show client-certificate UnknownIssuer and address-in-use errors. No migration completed. | [Run 36046899524](https://github.com/centerionware/not-k8s/actions/runs/36046899524) |
| 2026-09-24 | `bd0885951771fcc917d7b5a4a8ae01f3a0e148b8` | K3s+Cilium migration | Source checks and warning passed, but target bootstrap still enabled flanneld. This run confirmed that ignoring the backup filename alone was insufficient because the implicit Flannel default bypassed active-provider detection. | [Run 36048845512](https://github.com/centerionware/not-k8s/actions/runs/36048845512) |
| 2026-09-24 | `bd0885951771fcc917d7b5a4a8ae01f3a0e148b8` | Upstream Kubernetes+Cilium migration | Source checks and warning passed. Nodeapiserver failed to bind 6443 and nodestore reported UnknownIssuer errors. | [Run 36048845512](https://github.com/centerionware/not-k8s/actions/runs/36048845512) |
| Worktree after `bd088595` | — | Follow-up | Active external CNI detection now precedes K3s's implicit Flannel default, with a focused Cilium regression. The upstream 6443 handoff is still being traced. | — |
| 2026-09-24 | `29797f3f04489425e06b0f1b57bb0e4e619c0e3e` | K3s+Cilium integration | Full source setup, unattended warning, and target bootstrap passed; API import failed because dynamic list items were missing top-level `apiVersion` and `kind`. | [Run 36050278348](https://github.com/centerionware/not-k8s/actions/runs/36050278348) |
| 2026-09-24 | `29797f3f04489425e06b0f1b57bb0e4e619c0e3e` | Upstream Kubernetes+Cilium integration | Full source setup, unattended warning, and nodeapiserver bootstrap passed; API import failed on the same missing dynamic-object type metadata. | [Run 36050278348](https://github.com/centerionware/not-k8s/actions/runs/36050278348) |
| Worktree after `29797f3f` | — | Follow-up | Adds `apiVersion` and `kind` from Kubernetes discovery while exporting each dynamic API object. Focused CI checks passed; run 36051665474 passed source setup and target bootstrap, then exposed destination custom-API discovery failures recorded below. | [Run 36051665474](https://github.com/centerionware/not-k8s/actions/runs/36051665474) |
| 2026-09-24 | `c96505bdaae6387bb825c98faf77d2f2506c087c` | K3s+Cilium integration | Full source CNI/Cilium/CSI/workload checks, unattended warning, and target bootstrap passed. API import failed with 37 objects pending; final error was destination discovery missing `snapshot.storage.k8s.io/v1/VolumeSnapshotClass`. | [Run 36051665474](https://github.com/centerionware/not-k8s/actions/runs/36051665474) |
| 2026-09-24 | `c96505bdaae6387bb825c98faf77d2f2506c087c` | Upstream Kubernetes+Cilium integration | Full source checks, unattended warning, and nodeapiserver bootstrap passed. API import failed with 26 objects pending; final error was destination discovery missing `cilium.io/v2/CiliumEndpoint`. | [Run 36051665474](https://github.com/centerionware/not-k8s/actions/runs/36051665474) |
| 2026-09-24 | `c96505bdaae6387bb825c98faf77d2f2506c087c` | Docker five-node preflight | Image built; first systemd container exited 255 before readiness. | [Run 36051665474](https://github.com/centerionware/not-k8s/actions/runs/36051665474) |

| 2026-09-24 | `3fb149bc59b56cddc3b5f962215c738ad26e3b7d` | K3s+Cilium integration | Source checks, unattended warning, and target bootstrap passed. Import failed on 37 objects: destination discovery did not expose cert-manager, Cilium, K3s Addon, and snapshot APIs. Grouped errors are recorded in the artifact. | [Run 36053700863](https://github.com/centerionware/not-k8s/actions/runs/36053700863) |
| 2026-09-24 | `3fb149bc59b56cddc3b5f962215c738ad26e3b7d` | Upstream Kubernetes+Cilium integration | Source checks, unattended warning, and nodeapiserver bootstrap passed. Import failed on 26 objects: destination discovery lacked cert-manager, Cilium, and snapshot APIs; CertificateSigningRequest and ClusterTrustBundle application also failed. | [Run 36053700863](https://github.com/centerionware/not-k8s/actions/runs/36053700863) |
| 2026-09-24 | `3fb149bc59b56cddc3b5f962215c738ad26e3b7d` | Docker five-node preflight | Image build passed; first systemd container exited 255 before readiness. | [Run 36053700863](https://github.com/centerionware/not-k8s/actions/runs/36053700863) |
| 2026-09-24 | `99f232814f8a3ce2de2c71943eb4bf54a80a6930` | K3s+Cilium integration | Source checks, unattended warning, and target bootstrap passed. Import again failed on 37 objects across cert-manager, Cilium, K3s Addon, and snapshot APIs unavailable in destination discovery. CRD inventory output was absent because the binary has no tracing subscriber. | [Run 36055409069](https://github.com/centerionware/not-k8s/actions/runs/36055409069) |
| 2026-09-24 | `99f232814f8a3ce2de2c71943eb4bf54a80a6930` | Upstream Kubernetes+Cilium integration | Source checks, unattended warning, and nodeapiserver bootstrap passed. Import failed on 26 objects across cert-manager, Cilium, and snapshot APIs; CertificateSigningRequest and ClusterTrustBundle apply also failed. CRD inventory output was absent. | [Run 36055409069](https://github.com/centerionware/not-k8s/actions/runs/36055409069) |
| 2026-09-24 | `99f232814f8a3ce2de2c71943eb4bf54a80a6930` | Docker five-node preflight | Image build passed; first systemd container exited 255 before readiness. | [Run 36055409069](https://github.com/centerionware/not-k8s/actions/runs/36055409069) |
| 2026-09-24 | `cc020c97bebef3ea9c4618723710df607ccd5bda` | K3s+Cilium integration | Source checks and unattended warning passed; export captured 506 objects including 48 CRDs; target bootstrap passed. Import failed on 37 objects because destination discovery did not expose cert-manager, Cilium, snapshot, and K3s Addon APIs. | [Run 36056682815](https://github.com/centerionware/not-k8s/actions/runs/36056682815) |
| 2026-09-24 | `cc020c97bebef3ea9c4618723710df607ccd5bda` | Upstream Kubernetes+Cilium integration | Source checks and unattended warning passed; export captured 488 objects including 44 CRDs; target bootstrap passed. Import failed on 26 objects because destination discovery did not expose cert-manager, Cilium, and snapshot APIs; CertificateSigningRequest and ClusterTrustBundle operations also failed. | [Run 36056682815](https://github.com/centerionware/not-k8s/actions/runs/36056682815) |
| 2026-09-24 | `cc020c97bebef3ea9c4618723710df607ccd5bda` | Docker five-node preflight | Image build passed; first systemd container exited 255 before readiness. | [Run 36056682815](https://github.com/centerionware/not-k8s/actions/runs/36056682815) |
| 2026-09-24 | `336ea350502288d55bb6a57763494fa0aa89a475` | K3s+Cilium against v0.8.0 | Forward migration to nodestore completed and accepted all 48 CRDs; later fixture CSI redeployment reapplied imported snapshot CRDs, after which snapshot discovery returned 404. Reverse migration and return-state comparison were not reached. | [Run 36065059567](https://github.com/centerionware/not-k8s/actions/runs/36065059567) |
| 2026-09-24 | `336ea350502288d55bb6a57763494fa0aa89a475` | Upstream Kubernetes+Cilium against v0.8.0 | Import failed for CSR ExtraValue encoding and unsupported ClusterTrustBundle/v1. The source remained disabled and the protected export was retained; no reverse migration or state comparison ran. | [Run 36065059567](https://github.com/centerionware/not-k8s/actions/runs/36065059567) |
| 2026-09-24 | `336ea350502288d55bb6a57763494fa0aa89a475` | Focused server/utility checks | `quick-check` passed for `nodeapiserver,nodemigrate`; this verifies the current branch codec and CRD update changes, not the unmodified v0.8.0 runtime. | [Run 36065050713](https://github.com/centerionware/not-k8s/actions/runs/36065050713) |
| 2026-09-24 | `e6963be70467318f19b8c9fb8ef7197c0c9980b2` | K3s+Cilium against v0.8.0 | Forward migration completed; target VolumeSnapshotClass remained usable after the fixture stopped reapplying migrated CRDs. CSI controllers could not create pods because v0.8.0 returned 403 for `system:kube-controller-manager`; reverse migration and semantic comparison were not reached. | [Run 36066311951](https://github.com/centerionware/not-k8s/actions/runs/36066311951) |
| 2026-09-24 | `e6963be70467318f19b8c9fb8ef7197c0c9980b2` | Upstream Kubernetes+Cilium against v0.8.0 | Import failed for CertificateSigningRequest ExtraValue encoding and unsupported ClusterTrustBundle/v1; source was left disabled and export retained. | [Run 36066311951](https://github.com/centerionware/not-k8s/actions/runs/36066311951) |

Update this table after each implementation or verification change. Record the
exact SHA, workflow run, source distribution, CNI, and whether each stage
passed, failed, or was skipped.

The integration workflow currently defaults to K3s `v1.35.0+k3s1`, Cilium
`1.20.2`, cert-manager `v1.21.2`, and Helm `v3.17.3`. The kubeadm lane follows
the Kubernetes stable minor at dispatch time. Override the pinned values with
the workflow environment when testing a different compatibility combination,
and record the resolved versions in the run log.
