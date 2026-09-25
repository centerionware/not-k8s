# nodemigrate implementation and integration status

Last updated: 2026-09-25

This is the living implementation status record for the full scope in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). Capability marks describe code
that exists; verification marks describe evidence from a run. A passing
compile or unit test does not mark a real migration path as verified.

## Latest integration attempt

Branch-runtime migration [36165304330](https://github.com/centerionware/not-k8s/actions/runs/36165304330)
at SHA `f4e6fecfc121ed590ffa94380d3fcc5c4ee11edd` passed the nodemigrate and
combined-runtime builds and the five-node Docker isolation preflight. The
nodelet AppArmor fix was verified at runtime: CRI showed the Cilium
`mount-cgroup` container using `Unconfined` (`profile_type: 1`) and its log
reported `Mounted cgroupv2 filesystem`. K3s then failed Cilium startup because
orphaned source `cilium-agent` and `cilium-operator` processes held destination
ports `4244`, `4240`, and `9963`; the current worktree extends exact source
container/Pod identity cleanup to those daemons. The upstream lane failed
CertificateRequest import with HTTP 500 because the cert-manager webhook was
unreachable; rollback restored the source and retained the protected export.
Neither lane reached the target workload/storage checkpoint, reverse
migration, or parity comparison. Focused `nodelet` quick-check passed in
[36165304260](https://github.com/centerionware/not-k8s/actions/runs/36165304260).

## Previous runtime defect

At SHA `f97432ccc3edc65c846cb4c3c7bc7da3e30487f9`, branch-runtime K3s+Cilium
and upstream+Cilium run
[36150670405](https://github.com/centerionware/not-k8s/actions/runs/36150670405)
passed utility/runtime builds, the Docker isolation preflight, source fixtures,
and all CRD imports (58/58 K3s, 54/54 upstream). K3s forward migration
completed and the destination API passed readiness. Cilium Envoy then failed
binding its host socket with `errno=98`; CNI stayed uninitialized and the
hostPath CSI and CoreDNS checks could not proceed. Diagnostics show source
cleanup matched no process to the recorded source Cilium CRI IDs and removed
three stale sockets. The later host Envoy PIDs had cgroup CRI ID
`e14332ccaab8e20bdecfcff6bcc7140104799be59028628b3ebb15bcbd17e78a` and ancestor
shim ID `e512be6d65204325544d6ea9e454bd4f94e12617cde8e12bb630354239d7cf9f`,
which do not match the source IDs. Their ownership still needs tracing before
any cleanup change. Upstream import failed on CertificateRequest admission
because destination Cilium networking left the cert-manager webhook
unreachable; rollback restored the source service/API and retained the export.
No target workload checkpoint, reverse migration, or parity comparison passed.
Focused nodeapiserver quick-checks passed at map fix SHA `8555845f`
([36148848411](https://github.com/centerionware/not-k8s/actions/runs/36148848411))
and substring fix SHA `f97432cc`
([36150653684](https://github.com/centerionware/not-k8s/actions/runs/36150653684)).

The latest regular-release comparison, [run 36153673997](https://github.com/centerionware/not-k8s/actions/runs/36153673997)
at harness SHA `a35ab9ba8cba149613381a5a3738879205ce31fe`, used K3s
`v1.35.0+k3s1` and upstream Kubernetes `v1.37.1` as sources with the released
`v0.8.0` destination. The expanded source fixtures passed, the unattended
high-risk warning appeared, and nodemigrate captured 571/58 K3s objects/CRDs
and 551/54 upstream objects/CRDs. The released target accepted 55 and 51 CRD
apply requests respectively, but both lanes rejected the three Gateway API
CRDs because v0.8.0 lacks global CEL `matches`; the affected Gateway API v1
kind then remained unavailable. Upstream also failed restoring CertificateRequest
and CSR objects with HTTP 500. Both failures exercised rollback, which restored
the source API and retained the protected export. This release comparison does
not test the branch's CEL fixes as a destination; branch-runtime run
36150670405 confirms those CEL/map/substring fixes accept all source CRDs.
Neither release-backed lane passed a destination workload, reverse migration,
or round-trip parity checkpoint.

The next branch-runtime run at SHA `840e7ed8629edcd849729d5d7c0b1afb4f043f5d`
passed focused `nodemigrate` quick-check in [run 36156187333](https://github.com/centerionware/not-k8s/actions/runs/36156187333).
Migration [run 36156187055](https://github.com/centerionware/not-k8s/actions/runs/36156187055)
passed utility/runtime builds, the five-node Docker preflight, source fixtures,
and source Cilium cleanup. Cleanup stopped two leftover Envoy processes using
exact source container or pod identity and removed three stale sockets. K3s
target Envoy no longer reported the previous `errno=98` socket collision, but
the agent stayed `Init:1/6` at `mount-cgroup`; nodelet logs showed repeated
successful container creation without a captured start/exit result or
init-container status. CNI remained uninitialized, CoreDNS and CSI stayed
unavailable, and no target checkpoint, reverse migration, or parity assertion
ran. The harness now captures CRI inspect data and logs for every Cilium init
container by exact Pod UID, to expose the next failed lifecycle transition;
the underlying runtime cause is not yet established.

The upstream lane accepted all 54 CRD apply requests, then failed restoring
`cert-manager.io/v1/CertificateRequest migration-test-1` with HTTP 500 while
the target cert-manager webhook was unreachable. Rollback restored the source
and retained its protected export. Neither lane passed destination workload
parity or a round trip.

The diagnostic fix at SHA `8a08536ffb9809378b9ff806d2596f88445b0ac5` passed
focused `nodelet` quick-check in [run 36159523228](https://github.com/centerionware/not-k8s/actions/runs/36159523228).
Branch-runtime migration [run 36159532331](https://github.com/centerionware/not-k8s/actions/runs/36159532331)
passed both utility/runtime builds and the Docker five-node preflight. K3s
source fixture and Cilium cleanup passed; source Envoy process cleanup no
longer collides with destination Envoy. The target Cilium `mount-cgroup` init
container started, exited with code 1, and nodelet removed it before the final
CRI inspect/log collection. Nodelet's new correlated logs establish the exact
Pod/container/CRI ID and exit code. The worktree now watches this Cilium init
container every 500ms during target CSI setup to capture its output before
nodelet cleanup; that watcher awaits runtime verification. K3s CNI/CSI stayed
unavailable. Upstream again accepted all 54 CRD import requests and failed
CertificateRequest admission with HTTP 500 while its webhook was unreachable;
rollback restored source and retained the export. No target workload
checkpoint, reverse migration, or full parity comparison passed.

At SHA `4b3301af7713a90b4273275415c94e2164a1f5d2`, branch-runtime integration
[run 36119449016](https://github.com/centerionware/not-k8s/actions/runs/36119449016)
built nodemigrate and the combined runtime and passed the five-node Docker
preflight. K3s source checks passed, forward import accepted all 48 CRDs, and
the destination API passed readiness. The Cilium config init container
connected to the destination API and read `cilium-config`, but CNI remained
uninitialized and hostPath CSI setup failed before the target checkpoint. The
failure handler still queried the stopped source kubeconfig, so its unknown-CA
errors and `NotFound` Cilium Pod lookup did not reveal the destination agent's
state; the harness now switches to the target kubeconfig before post-cutover
work. Upstream again failed importing `CertificateRequest migration-test-1`
with HTTP 500 because the cert-manager webhook could not be reached while
destination CNI failed. Upstream rollback recovered the source API and retained
the protected export. No reverse migration or parity checkpoint passed.
The focused `nodebootstrap` quick-check passed the automatic API-address SAN
regression at SHA `8230c5f63faabada5393cc29cd3c6aed082177cb` in
[run 36120097438](https://github.com/centerionware/not-k8s/actions/runs/36120097438);
the runtime Cilium readiness effect remains unverified.

At SHA `459570ee4fdcea8128456f84058882159111a85a`, the focused nodemigrate
quick-check passed in [run 36104107511](https://github.com/centerionware/not-k8s/actions/runs/36104107511).
The K3s cleanup-order fix now gets past source sandbox shutdown: in both the
branch-runtime run [36104132045](https://github.com/centerionware/not-k8s/actions/runs/36104132045)
and the latest-release `v0.8.0` run
[36104248971](https://github.com/centerionware/not-k8s/actions/runs/36104248971),
nodemigrate imported all 48 discovered CRDs, reported destination API
readiness, and retained the protected export. Neither lane completed the
post-migration workload/storage checks or reverse migration. In both K3s
lanes, Cilium/CNI remained unready (`cni plugin not initialized`), CoreDNS and
the hostPath CSI Pods did not recover, and the fixture timed out waiting for
the CSI driver. The earlier `connection refused` to K3s's embedded containerd
did not recur.

The upstream lanes in both runs failed while importing
`cert-manager.io/v1/CertificateRequest migration-test-1`: the cert-manager
admission webhook could not be reached after target Cilium networking failed,
so the API server returned HTTP 500. Run `36104248971` against `v0.8.0` also
reproduced the known CSR `ExtraValue` protobuf error. The branch-runtime lane
did not report that codec failure. Its API/object import still did not complete
because of the CertificateRequest webhook failure. The release-backed failed
cutover restored the source API and retained the protected export. No reverse
migration, workload-parity checkpoint, or semantic round trip has passed.

Follow-up diagnostics in branch-runtime run
[36107163563](https://github.com/centerionware/not-k8s/actions/runs/36107163563)
at SHA `1a6f43162dae86349534129bae0217f34a36c398` confirmed that the process
holding the K3s Envoy sockets was left by the source runtime: its cgroup named
CRI container `ef96e5cf937fed840c1bfcc03df0ef667927c7f666ba4963da35faaa9f80f39a`
and its parent shim used `/run/k3s/containerd/containerd.sock` under
`k3s.service`. The worktree now records the Cilium Envoy CRI IDs while stopping
source sandboxes, then targets only Envoy processes whose cgroup matches those
exact IDs after disabling the source service. Cleanup failures restore the
source service. Unit regressions cover CRI metadata selection, systemd/cgroupfs
ID parsing, and process identity matching. Focused quick-check and runtime
retest initially hit a Rust `HashSet` borrowed-key type mismatch at SHA
`5736a03302b203e49cea66365f54b55d53e293a5`; the correction passed quick-check
in [run 36110023200](https://github.com/centerionware/not-k8s/actions/runs/36110023200).
The cgroup-only implementation stopped two Envoy processes in branch-runtime
K3s, but the target Envoy then could not connect to the Cilium agent's missing
`xds.sock`. Latest-release run [36110023663](https://github.com/centerionware/not-k8s/actions/runs/36110023663)
showed a surviving source Envoy whose cgroup container ID differed from the
source shim's `-id`. The worktree now includes both source Cilium agent and
Envoy CRI IDs and matches exact cgroup or ancestor-shim identities. Branch and
v0.8.0 migration runs at SHA
`aa4580f0938fb8c23b816c41b61d5e96c8314e4a` both stopped 2 stale source Envoy
processes and removed 3 stale sockets; the original source socket collision
did not recur. However, target Envoy initially could not connect to the
Cilium-agent `xds.sock`, CNI/CoreDNS stayed unready, and hostPath CSI did not
schedule. The upstream lanes still fail cert-manager webhook admission, with
v0.8.0 also reproducing the released CSR protobuf defect. One unit fixture
assertion failed at `aa4580f0` due to a negative CRI row named `cilium-agent`;
after correcting it to `cilium-operator`, the focused nodemigrate quick-check
passed at SHA `1faaf0634d89a6a116ce28d5a93374e6d082d54f` in [run
36112668805](https://github.com/centerionware/not-k8s/actions/runs/36112668805).
No run has passed reverse migration or full parity.

To diagnose the remaining target Cilium failure, the integration harness was
extended to capture current and previous `cilium-agent` and `config` container
logs when post-cutover storage setup fails. The first run revealed it selected
the stale source kubeconfig after cutover. The active-kubeconfig correction is
in SHA `ccb4d413ae61418dd3c9121e02992781f874cacd`; its branch-runtime rerun
[36116466142](https://github.com/centerionware/not-k8s/actions/runs/36116466142)
captured destination diagnostics. The selected Cilium Pod name returned
`NotFound`, while the available init-container output only reached “Establishing
connection to apiserver”; the Cilium agent stayed `PodInitializing`, and
containerd repeatedly failed CoreDNS sandbox networking with `cni plugin not
initialized`. This confirms the collection path is querying the destination,
but does not identify why the agent Pod lookup is stale or why Cilium fails to
initialize. Both K3s and upstream migration lanes failed; upstream again hit an
internal error importing `CertificateRequest migration-test-1`, and source
recovery/export retention passed. No reverse migration or parity checkpoint
passed. Logs: `/tmp/nodemigrate-36116466142/nodemigrate-k3s-36116466142/nodemigrate-k3s.log`
and `/tmp/nodemigrate-36116466142/nodemigrate-kubernetes-36116466142/nodemigrate-kubernetes.log`.

## Required migration test inventory

The acceptance fixture must migrate and verify the following resource groups
at the initial source, not-k8s target, and returned-source checkpoints. Rows
marked partial describe current fixture setup only; no row is complete until
the same object/state and behavior assertions pass through a full round trip.

| Resource group | Required migration and behavior checks | Current fixture status |
| --- | --- | --- |
| Configuration and identity | Namespaces, ConfigMaps (including binary data), Secrets, ServiceAccounts, Roles, ClusterRoles, RoleBindings, ClusterRoleBindings, ResourceQuotas, LimitRanges, and PriorityClasses; verify identity/data and allowed plus denied requests using real service-account credentials. | The fixture includes binary/text ConfigMaps, a Secret consumed through Pod environment, namespaced and cluster-scoped RBAC, and real-token Jobs for allowed ConfigMap/Node reads and denied Secret reads. It now checks ResourceQuota, LimitRange, and PriorityClass state at every stage. Additional RBAC and quota behavior cases plus runtime round-trip verification remain pending. |
| Workload controllers | Deployments, ReplicaSets, StatefulSets and `volumeClaimTemplates`, DaemonSets on every node, Jobs, CronJobs, and standalone Pods; verify selectors, templates, rollout/revision history, replica/readiness counts, job execution, schedule, and unique application data at each checkpoint. | The fixture includes an nginx Deployment, a StatefulSet backed by a CSI claim template with a seeded payload, an all-node DaemonSet, a completed Job, a manually triggered CronJob, and a standalone data-seed Pod. Both source lanes passed these checks and invoked nodemigrate in run 36142617576. Target and returned-source checks remain unverified because destination import rolled back on Gateway API admission-policy evaluation. |
| Helm-managed applications | Helm charts/releases and release records (name, namespace, chart/version, values, revision, manifest), plus all chart-managed resources; verify `helm list`, release inspection, workload health, and a safe follow-up Helm operation after each cutover. | Traefik, cert-manager, and Cilium are installed by Helm. Source-stage checks inspected values, manifests, history, and version-pinned server-side dry-run upgrades of Traefik and Cilium in both lanes of run 36140783446. Post-migration release state and follow-up operations remain unverified. |
| Service networking and ingress | Services, Endpoints where served, EndpointSlices, IngressClasses and Ingresses, NetworkPolicies, and Gateway API `GatewayClass`, `Gateway`, `HTTPRoute`, `GRPCRoute`, `TCPRoute`, `TLSRoute`, and `UDPRoute` where supported; verify DNS, service reachability, policy allow/deny, HTTP/TLS routing, and certificate use. | Both source lanes reported accepted/programmed Gateway and HTTPRoute state and passed behavior probes after the fixture targeted Traefik's configured 8080 Gateway listener (run 36142617576). Destination Gateway CRD import was rejected by nodeapiserver because the `safe-upgrades` ValidatingAdmissionPolicy used unsupported global CEL `matches`; route behavior on the target and return source remains pending. NetworkPolicy behavior and broader Service/EndpointSlice parity remain absent. |
| Persistent storage | PVs, PVCs, StorageClasses, static hostPath/local volumes, dynamic CSI volumes, CSIDrivers, CSINodes, VolumeAttachments where applicable, VolumeSnapshotClasses, VolumeSnapshots, snapshot contents, and StatefulSet claim templates; verify binding, topology, provider configuration/credentials, attachment behavior, and unique payload data. | Static hostPath and dynamic hostPath CSI PVC data remain seeded; the StatefulSet now also owns a dynamic CSI claim template with a distinct payload checked at every checkpoint. The current Cilium failure prevents target storage checks; provider and multi-node attachment coverage is absent. |
| CRDs and operator resources | CRDs plus representative custom resources and durable operator state, including Cilium configuration/policy, cert-manager Issuers, ClusterIssuers, Certificates and CertificateRequests, and Gateway API resources; verify discovery, version conversion, reconciliation, status, and resulting Secrets/routes. | Cilium, cert-manager, and Gateway API CRDs/custom resources are sampled and Gateway API resources now have route status/behavior checks. Broad custom-resource coverage and successful target reconciliation are unverified. |
| Policies, admission, and scheduling | PodDisruptionBudgets, autoscalers, NetworkPolicies, validating/mutating webhook configurations, admission policies, APIService registrations where present, RuntimeClasses, node selectors/affinity, tolerations, topology spread, and other supported constraints; verify stored configuration and observable decisions. | Not yet represented as a systematic test matrix. |
| Full API inventory | Enumerate source discovery and every listable API resource, including Events, Leases, coordination/discovery objects and token/request resources. Compare durable object identity/spec/data after migration; classify each other kind as migrated, regenerated transient state, or excluded with a specific Kubernetes lifecycle reason. Add safe-to-create discovered kinds to the fixture. | The harness captures a source object inventory and semantic checkpoint, but completeness and migration parity have not passed a full round trip. |

The list above is a minimum. Every named resource group must be tested for
migration and behavior at all three checkpoints; a source setup or successful
API import is not a passing test. Any additional API kind, add-on, data path,
or controller behavior found in source discovery or real workloads must be
added to this matrix and exercised unless its lifecycle is explicitly
documented.

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
| 2026-09-25 | `d739633a15a330565339b7028e9339e73326b207` | K3s+Cilium and upstream+Cilium branch-runtime lanes | K3s source assertions and forward API import passed (48 CRDs); destination API became ready. Target Envoy failed with `errno=98`, CNI remained uninitialized, and CSI setup failed. Upstream import hit the unavailable cert-manager webhook; source rollback and protected export passed. Neither lane reached target workload parity or reverse migration. | [Run 36121960626](https://github.com/centerionware/not-k8s/actions/runs/36121960626); logs `/tmp/nodemigrate-36121960626/` |
| 2026-09-25 | `217bf87275f1abbacdcb1f6e587b5914f72f3f92` | Expanded fixture and required resource inventory | Fixture now covers binary ConfigMaps/Secrets, RBAC allow/deny using real service-account credentials, Job/CronJob, all-node DaemonSet, CSI-backed StatefulSet claim template, Helm release state/follow-up, and Gateway API routing. Broader inventory includes all API discovery kinds and listed Kubernetes resources. Focused shell/snapshot checks passed; runtime result pending. | Dedicated migration workflow pending |
| 2026-09-25 | `217bf87275f1abbacdcb1f6e587b5914f72f3f92` | Branch-runtime migration and five-node Docker preflight | Nodemigrate and combined runtime builds passed, and Docker isolation preflight passed. Both source migration lanes failed in their `Run migration` steps; completed job metadata does not report the failing operation. GitHub API connectivity failed during log retrieval, so the first failing assertion and whether the expanded workload checks ran are unknown. | [Run 36124385324](https://github.com/centerionware/not-k8s/actions/runs/36124385324); artifact logs not yet retrieved |
| 2026-09-25 | `217bf87275f1abbacdcb1f6e587b5914f72f3f92` | K3s+Cilium and upstream+Cilium source setup | Both lanes installed and reached the source fixture. Cilium, hostPath CSI, cert-manager, and nginx were available. GatewayClass was Accepted, but `Gateway/migration-traefik` did not become Programmed within two minutes; the source fixture failed before nodemigrate ran. No migration stage or resource parity was tested. | [Run 36124385324](https://github.com/centerionware/not-k8s/actions/runs/36124385324); logs `/tmp/nodemigrate-36124385324/` |
| 2026-09-25 | `404d68f7b3aa6dc04192e2061e48f563ae2d5589` | Expanded source fixture | Added cluster-wide RBAC, quota/limit/priority objects and checks, real ConfigMap/Secret consumption, and Cilium Helm release verification. Added a Traefik readiness barrier and Gateway-specific failure diagnostics after run 36124385324 timed out before migration. Shell and snapshot-filter checks passed; runtime recheck is pending. | Dedicated migration workflow pending |
| 2026-09-25 | `7899ac9e1d2d6140d4b799bf64beed1657ac6c1c` | Expanded branch-runtime fixture | Both K3s and kubeadm source installs reached the fixture but timed out waiting for `migration-seed`; no nodemigrate command ran. The kubeadm lane's final events showed `Insufficient cpu` for fixture workloads, and Gateway status showed Traefik has no matching entryPoint for listener port 80. Cilium, hostPath CSI, cert-manager, and Traefik were running; PVCs were Bound. | [Run 36126850738](https://github.com/centerionware/not-k8s/actions/runs/36126850738); logs `/tmp/nodemigrate-36126850738/` |
| 2026-09-25 | `f2d2c7639b78ee1a1cb9b6609903e6374fb24716` | Corrected branch-runtime fixture | Both source clusters passed PVC and seed Pod readiness, the nginx Deployment rolled out, and the port 8080 Gateway became Programmed. Both then timed out waiting for HTTPRoute `Accepted`. The upstream route's nested parent conditions already reported `Accepted=True` and `ResolvedRefs=True`; the harness used an unsupported top-level condition wait. Neither lane invoked nodemigrate. | [Run 36129138298](https://github.com/centerionware/not-k8s/actions/runs/36129138298); logs `/tmp/nodemigrate-36129138298/` |
| 2026-09-25 | `b67692295b72763f5706d59774bda7d6527f51eb` | HTTPRoute condition wait correction | Both source fixtures passed Gateway and HTTPRoute readiness after switching to nested-parent condition polling. Each then failed at PriorityClass validation because `$stage` was unset in `install_workloads` under `set -u`; neither lane invoked nodemigrate. | [Run 36130714079](https://github.com/centerionware/not-k8s/actions/runs/36130714079); logs `/tmp/nodemigrate-36130714079/` |
| 2026-09-25 | `b67692295b72763f5706d59774bda7d6527f51eb` | HTTPRoute condition wait correction | Both source fixtures passed Gateway and HTTPRoute readiness after switching to nested-parent condition polling. Each then failed at PriorityClass validation because `$stage` was unset in `install_workloads` under `set -u`; neither lane invoked nodemigrate. | [Run 36130714079](https://github.com/centerionware/not-k8s/actions/runs/36130714079); logs `/tmp/nodemigrate-36130714079/` |
| 2026-09-25 | `4c05f5c73b9fb77e65783540e6766fd2ec56a544` | Source fixture stage initialization | Both source fixtures passed Gateway and HTTPRoute readiness and ran the source PriorityClass assertion. It failed in both lanes because the API omitted its default false `globalDefault` field; no nodemigrate command ran. | [Run 36131837328](https://github.com/centerionware/not-k8s/actions/runs/36131837328); logs `/tmp/nodemigrate-36131837328/` |
| 2026-09-25 | `89ffb03f46f29dec02b9fb3cff5cf22a3c127013` | PriorityClass API default handling | Both lanes passed source fixture setup, PriorityClass, Gateway, and workload assertions. They then failed because jq parsed the hyphenated Secret data key as invalid dot notation; no nodemigrate command ran. | [Run 36133100159](https://github.com/centerionware/not-k8s/actions/runs/36133100159); logs `/tmp/nodemigrate-36133100159/` |
| 2026-09-25 | `de7d79333131c566dd1260e9cdcbcf159d8a33a4` | Secret key selector correction | Both source lanes passed fixture setup, Secret/ConfigMap checks, persistent data, Job, and CronJob checks. They then failed while applying the RBAC test Jobs because the source heredoc executed `$(NODE_NAME)` locally. | [Run 36134496060](https://github.com/centerionware/not-k8s/actions/runs/36134496060); logs `/tmp/nodemigrate-36134496060/` |
| 2026-09-25 | Follow-up to `de7d79333131c566dd1260e9cdcbcf159d8a33a4` | RBAC Job heredoc escaping | The heredoc now preserves the Node-name command substitution for the Job container. This is test-harness work only; migration behavior remains unverified pending rerun. | Not dispatched yet |
| 2026-09-25 | `b48511d3c393ecdc9e1c0126210267d62946d266` | RBAC Job expansion | Both source lanes passed prior fixture checks, but the three RBAC probe Pods remained Pending with scheduler events reporting `Insufficient cpu`; no nodemigrate command ran. | [Run 36136419993](https://github.com/centerionware/not-k8s/actions/runs/36136419993); logs `/tmp/nodemigrate-36136419993/` |
| 2026-09-25 | Follow-up to `b48511d3c393ecdc9e1c0126210267d62946d266` | Helper Pod resource requests | The RBAC and persistent-data helper containers now request 1m CPU/1Mi memory. This is test-harness work only; migration behavior remains unverified pending rerun. | Not dispatched yet |
| 2026-09-25 | `64ea1fa3f0975b4087b4f2b126cd8996c53f23b5` | RBAC helper resource requests | Both source lanes passed the previously blocked RBAC Jobs after setting 1m CPU/1Mi requests on helper containers. They then failed the Traefik Ingress/Gateway HTTP traffic probe at the source checkpoint; Gateway and HTTPRoute statuses were accepted/programmed, but the harness discarded response bodies/status codes. The nodemigrate utility was not invoked and no migration parity assertion ran. | [Run 36138657733](https://github.com/centerionware/not-k8s/actions/runs/36138657733); logs `/tmp/nodemigrate-36138657733/` |
| 2026-09-25 | Follow-up to `64ea1fa3f0975b4087b4f2b126cd8996c53f23b5` | Traefik probe diagnostics and API inventory | Capture HTTP status/body and dump Traefik plus source service/endpoints and route details on failure. The goal document also names additional discovered API kinds and makes the all-discovered-resource rule explicit. Local shell, snapshot, and diff checks passed; runtime validation is pending. | Not dispatched yet |
| 2026-09-25 | `36fb2e70852ef4711db60128d29c483b115e41dc` | Gateway listener fixture correction | Both source lanes passed all workload, storage, cert, Helm, RBAC, and HTTP/Gateway behavior checks. Both invoked nodemigrate. Destination import then failed on Gateway API v1.6.1's `safe-upgrades` ValidatingAdmissionPolicy: nodeapiserver reported undeclared global CEL `matches` while creating three CRDs. Upstream also reported an internal error applying CertificateRequest `migration-test-1`. Rollback restored source service/API and kept the protected export for both. | [Run 36142617576](https://github.com/centerionware/not-k8s/actions/runs/36142617576); logs `/tmp/nodemigrate-36142617576/artifacts/` |
| Follow-up to `36fb2e70852ef4711db60128d29c483b115e41dc` | nodeapiserver global CEL `matches` | Add the missing global regex function used by Gateway API's safe-upgrades policy and an expression-level regression. Local formatting/snapshot checks and focused `nodeapiserver` CI plus runtime rerun are pending. | Not dispatched yet |
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
