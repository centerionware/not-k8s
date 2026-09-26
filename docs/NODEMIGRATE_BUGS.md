# nodemigrate bug and fix tracker

Last updated: 2026-09-26

## Latest diagnostic update

Migration run [36248706224](https://github.com/centerionware/not-k8s/actions/runs/36248706224)
used SHA `b14185a1e484fca6d0b986bcf01219fe822ff149`. Focused
`nodecontroller` quick-check [36248706148](https://github.com/centerionware/not-k8s/actions/runs/36248706148)
passed. Both migration lanes now restore the imported static PVC to `Bound`,
confirming the binder fix. They then timed out waiting for
`Pod/migration-data-check`: nodelet's PVC resolver only mounted CSI-backed PVs
and left the fixture's bound `hostPath` PV unresolved. This confirms a separate
nodelet storage bug. The current worktree adds direct hostPath/local PV path
resolution with validation and focused source extraction tests; quick-check and
migration retest are pending. No return migration ran. Logs and artifacts:
`/tmp/nodemigrate-36248706224-{k3s,kubernetes}-job.log` and
`/tmp/nodemigrate-36248706224-artifacts/`.

Follow-up to [36241224151](https://github.com/centerionware/not-k8s/actions/runs/36241224151):
the upstream hostpath CSI driver keeps volume state and payload under
`/csi-data-dir`, and its deployment only guarantees persistence while the
plugin StatefulSet Pod remains up ([driver documentation](https://github.com/kubernetes-csi/csi-driver-host-path/blob/master/docs/deploy-1.17-and-later.md)).
The migration fixture now backs that directory with node-local persistent
storage before creating claims, so stopping and recreating the CSI Pod during
an in-place runtime migration retains the provider's source volume inventory
and bytes. `bash -n` and `git diff --check` pass; runtime migration validation
is pending. This addresses the hostpath test provider's restart lifecycle; it
does not verify moving CSI backend data to a different physical node or
transfer for other CSI providers. Those remain part of the open storage bug.

Migration run [36241224151](https://github.com/centerionware/not-k8s/actions/runs/36241224151)
on SHA `606f46f624007dfd6215be26b97e54817fe020af` root-caused the CSI mount
failure in both K3s+Cilium and upstream Kubernetes+Cilium. The source and
target PVs retained the same CSI volume handle, but each freshly installed
hostpath CSI driver's volume inventory did not contain that handle. The CSI
controller returned `NotFound: volume id ... does not exist in the volumes
list` to repeated `NodeStageVolume` calls, and the StatefulSet Pod stayed
Pending. The PV's hostpath topology label still matched the destination Node;
affinity is not the blocker. `nodemigrate` currently snapshots `hostPath` and
`local` PV filesystem paths only; it neither moves CSI payloads nor recreates
provider volume state. This is a confirmed nodemigrate storage migration bug,
not a nodelet mount or scheduler bug. Fix must transfer data through a
provider-supported path and create/restore a destination volume before
importing dependent claims/workloads, while retaining a recoverable source
copy. No CSI data parity, semantic checkpoint, reverse migration, or full
round trip passed. Artifacts: `/tmp/nodemigrate-36241224151/`.

Migration run [36239708922](https://github.com/centerionware/not-k8s/actions/runs/36239708922)
reproduced the StatefulSet CSI mount stall in both source lanes. The target PV
had required node affinity `topology.hostpath.csi/node In [runnervmtr4k5]`,
and the target Node had the same label/value. The Pod's active status was
`waiting for CSI volume(s) to be mounted: state`; the PVC and PV were Pending
at failure. Earlier `FailedScheduling` events cited volume affinity conflict,
but the final PV/Node data does not show a mismatch, so scheduler affinity is
not a confirmed cause of the final stall. Run `36239708922` did not capture
nodelet or hostpath CSI logs. The current harness captures those logs and the
source/target fixture PV/PVC specs; the next run must identify the CSI call or
state transition that remains pending before component ownership or a fix can
be established. No full round trip is verified. Artifacts:
`/tmp/nodemigrate-36239708922/`.

Migration run [36238216668](https://github.com/centerionware/not-k8s/actions/runs/36238216668)
confirms the generation fix: both targets reported StatefulSet
`generation=1`/`observedGeneration=1`. It exposes a separate unresolved storage
failure in both lanes. The StatefulSet pod was Pending with
`FailedScheduling: 0/1 nodes are available: 1 node(s) had volume node affinity
conflict`; its StatefulSet PVC showed `Pending` and capacity `0` even though
the target's PV/PVC table displayed `Bound`. The hostpath CSI plugin and its
socat pod were Running, and the separate CSI readiness PVC bound successfully.
The target Node included matching `kubernetes.io/hostname` and
`topology.hostpath.csi/node` labels. The exact PV affinity was not captured in
that run; run `36239708922` captured it and found an exact match. The
integration harness was extended with PV YAML and target Node labels. The
unresolved issue moved from suspected affinity mismatch to a CSI mount still
pending at failure. No fix or round trip is verified. Logs:
`/tmp/nodemigrate-36238216668/`.

Run [36236810283](https://github.com/centerionware/not-k8s/actions/runs/36236810283)
root-caused the shared StatefulSet rollout timeout. Both lanes showed
`metadata.generation=null` and `status.observedGeneration=null`; the stateful
controller otherwise reported the object and template revisions. nodemigrate
imports with server-side apply, whose create-on-apply path omitted the
server-owned generation that ordinary POST creation sets. The branch now
initializes generation 1 on both create paths and adds a focused regression.
`nodeapiserver` quick-check and the full migration rerun are pending. Logs:
`/tmp/nodemigrate-36236810283/`.

Migration run [36235423620](https://github.com/centerionware/not-k8s/actions/runs/36235423620)
on SHA `d177f2c664e2b661da65d43275002bf79cf71b65` confirms the prior
GatewayClass WATCH-revision fix: both source lanes passed GatewayClass
Accepted and Gateway Programmed. Both then timed out waiting for
`StatefulSet/migration-stateful` to observe its spec generation; its ordinal
Pod was Pending at failure. Component ownership and cause are not confirmed.
The current logs omit both `.metadata.generation` and
`.status.observedGeneration`, so this is tracked as an unresolved runtime
failure, not yet a confirmed component bug. The harness now captures those
fields, the StatefulSet/Pod/PVC descriptions, and nodecontroller journal output
for the next attempt. No fix or full round trip is verified. Logs:
`/tmp/nodemigrate-36235423620/`.

The repeated GatewayClass 409 in [run 36234240173](https://github.com/centerionware/not-k8s/actions/runs/36234240173)
is now root-caused. For streaming-list WATCH, the handler created synthetic
initial ADDED events with the collection snapshot revision instead of each
object's `mod_revision`. Traefik then submitted an RV newer than the target
object's stored revision (`1382` vs `1049` in K3s; `891` vs `889` upstream),
and nodeapiserver correctly returned 409. The source now gives each initial
event its object's revision and retains the snapshot revision for the
initial-events-end bookmark. Regression added; focused nodeapiserver CI and
runtime validation are pending. The exact source logs are in
`/tmp/nodemigrate-36234240173/`.

Branch-runtime run [36232994720](https://github.com/centerionware/not-k8s/actions/runs/36232994720)
at SHA `56f27d13d05af278706f280f69c29f3d8d1bd195` passed focused nodemigrate
checks [36232987555](https://github.com/centerionware/not-k8s/actions/runs/36232987555),
the five-node preflight, and both scoped builds. Both source fixtures passed;
both migrations reached destination API readiness. K3s target Node/Cilium,
fixture DaemonSet/nginx rollout, namespace CA, and CSI PVC readiness passed.
Both lanes then stalled at GatewayClass readiness. Audit events identify
Traefik's service account issuing GatewayClass `/status` updates and receiving
HTTP 409 repeatedly; the stored condition remains `Accepted=Unknown` with
`Waiting for controller`. The log lacks submitted/current resourceVersion
values, so whether the client is stale or the CRD status update path compares
the wrong revision remains unverified. The previous `crd` verifier error and
K3s CRI stop timeout did not recur. No target semantic checkpoint or return
migration passed.

Branch runtime [36231779729](https://github.com/centerionware/not-k8s/actions/runs/36231779729)
at SHA `df31d039178266efb513eb265781f10aa498eeb2` passed its Docker preflight
and scoped builds. K3s reached nodemigrate with 582 captured objects, then
`crictl stopp` returned `DeadlineExceeded`; source rollback and export
retention passed. Upstream migration completed and the destination API became
ready, but the harness failed at `kubectl get crd` because the not-k8s API does
not advertise that kubectl shortcut. Neither lane completed a round trip.
The branch now verifies the full API resource name and checks CRI's running
container list before deciding whether a failed stop is safe to tolerate.
Both fixes await focused CI and runtime validation.

Branch-runtime migration [36229667964](https://github.com/centerionware/not-k8s/actions/runs/36229667964)
at SHA `294a6c64` provides new target-time evidence. In the K3s lane, the
mounted CA fingerprint read from the live Cilium Pod exactly matched the
destination API CA; Cilium agent, Envoy, and operator reached `1/1`, and the
Node was Ready. CoreDNS remained `Running` but `0/1 Ready`; nodelet journal
output confirms the CoreDNS gate kept ordinary Pod reconciliation paused,
which prevented hostpath CSI and other workload checks from proceeding.
CoreDNS logs say the Kubernetes plugin was waiting for API synchronization
and the ready plugin was not ready. They do not identify the failing API
request or distinguish network, TLS, or authorization failure. Upstream imported all 55
CRDs but then received HTTP 500 importing `CertificateRequest/migration-test-1`
while the cert-manager webhook had no endpoints. Both lanes recovered source
service and retained exports. No round trip passed. This narrows the K3s
problem away from Cilium's sampled CA trust; do not mark CNI or workload
recovery fixed until the full runtime scenario passes. Source comparison then
found the target API Service EndpointSlice still pointed to `127.0.0.1` while
the Node had a reachable `10.1.0.61` InternalIP. Bootstrap refreshed this
endpoint only for Flannel; external-CNI migration therefore left CoreDNS
without a Pod-reachable API endpoint. The current branch now refreshes the
nodeapiserver endpoint to the explicit advertise address or detected host IP
for non-Flannel CNI. Focused nodebootstrap CI and migration rerun are pending.

Migration run [36226000144](https://github.com/centerionware/not-k8s/actions/runs/36226000144)
validated the corrected byte comparison: both source-stage checks passed, and
target snapshots repeatedly showed every namespace root CA ConfigMap matching
the destination kubeconfig. The original runtime symptom remained: K3s Cilium
and Traefik rejected the target API certificate, and both lanes failed
`CertificateRequest/migration-test-1` admission because the cert-manager
webhook was unreachable. Source rollback and protected-export retention
passed. This proves the ConfigMap contents alone do not establish what CA a
running Pod loaded. A likely lifecycle race is that workload controllers are
applied before the root CA publisher has populated the destination namespace;
the importer now stages Namespace creation and creates or corrects matching
destination CA bundles before applying remaining resources. This fix is pending focused CI and a
runtime rerun; no root cause is claimed yet.

Migration run [36224872002](https://github.com/centerionware/not-k8s/actions/runs/36224872002)
stopped both lanes at the source-stage CA fixture assertion, before invoking
`nodemigrate`. The assertion compared decoded PEM via shell command
substitution, which strips trailing newlines and likely caused the false
mismatches. The fixture now compares base64-encoded bytes and passed at source
and target stages in [run 36226000144](https://github.com/centerionware/not-k8s/actions/runs/36226000144).
This is a
confirmed test-harness defect, not evidence that migration or target CA
regeneration failed. Focused nodemigrate tests passed on SHA `cf1b984a` in
[run 36224841930](https://github.com/centerionware/not-k8s/actions/runs/36224841930).

Dedicated migration run [36223445443](https://github.com/centerionware/not-k8s/actions/runs/36223445443)
used SHA `9e8701291e2cd8f821f53e1e43d0bc306fd2f035`. Builds and five-node
preflight passed; both migration lanes failed before workload parity or
reverse migration. Nodeproxy journals show it started and watched Services and
EndpointSlices; the inactive status was recorded after rollback stopped it.
K3s Cilium and Traefik controllers repeatedly reported TLS verification errors
(`x509: certificate signed by unknown authority`) while reaching the target
API Service. Nodemigrate currently exports the source `kube-root-ca.crt`
ConfigMaps, which can leave Pods trusting the source CA after target bootstrap.
This is a strong cause candidate, but the run did not capture the ConfigMap
data to directly confirm the mismatch. The current fix regenerates this
destination-managed ConfigMap and the fixture checks all namespace bundles
against the active API CA. Runtime confirmation is pending. Upstream still
failed its CertificateRequest import with HTTP 500, then recovered its source
and retained its protected export. Logs are under
`/tmp/nodemigrate-36223445443/`.

Release-backed migration [36228920539](https://github.com/centerionware/not-k8s/actions/runs/36228920539)
at SHA `f86fad5622d48582c24531df4b88368096d632d9` fetched `v0.8.0`. Both
nodemigrate builds and Docker preflight passed; both lanes failed during
forward import and recovered their sources while retaining protected exports.
K3s captured 582 objects/59 CRDs, but v0.8.0 rejected three Gateway API CRDs
for unsupported CEL `matches`. Target snapshots reported no schedulable Nodes,
no running Cilium/cert-manager Pods, and no webhook endpoints. The mounted-CA
probe had no usable Pod: `kubectl exec` failed because nodelet port 10250
refused connections. The source Node returned Ready after rollback. This is
stronger evidence for a target Node/runtime readiness failure than a CA
mismatch; the run did not establish the bytes mounted in Cilium. Upstream also
failed CertificateRequest and CSR imports with HTTP 500 and hit the same
Gateway API CRD incompatibility. Neither lane reached target workload checks,
reverse migration, or parity. The watcher now records target Node conditions
and reports a failed mounted-CA probe explicitly. Artifacts:
`/tmp/nodemigrate-36228920539/`.

Release-backed migration [36228127861](https://github.com/centerionware/not-k8s/actions/runs/36228127861)
at SHA `d92abf28397e143c266c115cf2deb7d694f23374` fetched the regular
`v0.8.0` runtime. Both source CA checks and the Docker preflight passed, but
both lanes failed during forward import and recovered their sources while
retaining protected exports. K3s Cilium/Traefik still reported API TLS
`unknown authority` after destination CA ConfigMaps were seeded and matched.
This confirms the ConfigMap value alone does not establish what CA a running
Pod has mounted; the harness now records service-account CA fingerprints for
Cilium and cert-manager. Upstream also returned HTTP 500 for CertificateRequest
and CSR import, while v0.8.0 rejected three current Gateway API CRDs because
its CEL runtime does not support the policy's `matches` function. No target
workload checkpoint, reverse migration, or parity comparison passed. Logs:
`/tmp/nodemigrate-36228127861/`.

Dedicated migration run [36220533297](https://github.com/centerionware/not-k8s/actions/runs/36220533297)
used branch head `ed851766d59ac3b424c1f28e89808c5d7415ad92`. The Docker
preflight and both utility/runtime builds passed; both migration lanes failed.
K3s reached destination API readiness, then target hostPath CSI setup failed.
The initial target snapshot showed the Cilium agent Ready, with Envoy and
CoreDNS Running but not Ready. Failure-time diagnostics later showed Cilium
agent/Envoy/operator `1/1`, while CoreDNS remained `0/1 Running`. Cert-manager
Pods had no phase/container status/IP and its webhook had no endpoints. Upstream
failed applying `CertificateRequest/migration-test-1`; its target webhook
Service and EndpointSlice had no endpoints and cert-manager Pods had no phase,
container status, or Pod IP. Rollback restored the upstream source API and
retained its protected export. Neither lane reached target workload,
reverse-migration, or parity checks. The target symptoms do not yet identify
their cause. Logs: `/tmp/nodemigrate-36220533297-k3s.log` and
`/tmp/nodemigrate-36220533297-kubernetes.log`.

Run 36220533297 validates the watcher correction: normalized state sampling
recorded 10 K3s and 9 upstream snapshots, with bounded periodic diagnostics
instead of AGE-driven repeated bundles. Static validation was skipped by the
manual dispatch; the branch had already passed `bash -n` and `git diff --check`.

Latest branch-runtime migration [36217850294](https://github.com/centerionware/not-k8s/actions/runs/36217850294)
used head SHA `d7b65846f55f24e75fd56ca54d107f5bad8b5511`. K3s accepted its
CiliumNode update with HTTP 200, but both migration lanes failed importing
`CertificateRequest/migration-test-1` because the cert-manager webhook could
not be reached (HTTP 500). The CoreDNS and Cilium pod diagnostics ran after
nodemigrate restored the source; they are post-rollback source state and do not
identify the failed target's condition. A target-state watcher has been added
to sample Cilium, CoreDNS, cert-manager pods, and webhook endpoints during
forward migration; runtime validation is pending. Both lanes retained their
protected exports; neither reached target workload checks, reverse migration,
or parity. Logs:
`/tmp/nodemigrate-36217850294-k3s.log` and
`/tmp/nodemigrate-36217850294-kubernetes.log`.

Previous run [36216429427](https://github.com/centerionware/not-k8s/actions/runs/36216429427)
also observed a successful CiliumNode update and no 409 conflict. Its K3s
target failed during hostpath CSI setup with three CoreDNS pods `Running` but
`0/1` Ready and most other pods `Unknown`; that failure artifact did not
include CoreDNS descriptions or logs. Logs:
`/tmp/nodemigrate-36216429427-k3s.log` and
`/tmp/nodemigrate-36216429427-kubernetes.log`.

Latest completed branch-runtime migration [36195385046](https://github.com/centerionware/not-k8s/actions/runs/36195385046)
used SHA `6037e67e1f4e0a9d8d365d5ce8653c572c29ed72`. In K3s, all source
checks, all 59 CRD imports, and destination API readiness passed. The
`mount-bpf-fs` CRI stop event carried Pod metadata and the later CRI inventory
showed `CONTAINER_EXITED`, exit code 0, but no subsequent init container was
created and the Pod status remained Running. The run did not reproduce the
CiliumNode 409. Upstream accepted all 55 CRDs, then CertificateRequest
admission returned HTTP 500 after destination Cilium networking disappeared.
Both lanes restored the source and retained the protected export; neither
reached target workloads, reverse migration, or parity. Focused nodeapiserver
quick-check [36195027134](https://github.com/centerionware/not-k8s/actions/runs/36195027134)
passed; focused nodelet quick-check [36197970777](https://github.com/centerionware/not-k8s/actions/runs/36197970777)
passed on SHA `9974e763`. The branch-runtime rerun
[36197970992](https://github.com/centerionware/not-k8s/actions/runs/36197970992)
completed with both migration steps failed after the scoped builds passed; its
Docker isolation preflight also passed. Its K3s log shows Cilium reached
`mount-bpf-fs`, whose output confirmed bpffs was mounted, but the CRI task later
disappeared without advancing the Pod past `Init:3/6`. CoreDNS then repeatedly
failed sandbox setup when `cilium-cni` returned `signal: killed`. The target
also produced 6,376–8,366 `resourcequotas` range calls per 30 seconds and
1,167 `migration-quota/status` PATCH audit records in about 1.2 seconds. The
fixture quota has both `pods` and unsupported `requests.storage` keys,
exposing a ResourceQuota controller whole-map comparison against its partial
(`pods`/`services`) result: merge-patching preserved the unsupported key and
caused an endless status-write loop. The upstream lane also exceeded 6,800
ResourceQuota range calls per 30 seconds and failed CertificateRequest import
because the cert-manager webhook was unreachable after target CNI failed. Both
rollback paths restored the source and retained the protected export. Full job
logs: `/tmp/nodemigrate-361979-k3s.log` and
`/tmp/nodemigrate-361979-kubernetes.log`.

The next migration [36189354168](https://github.com/centerionware/not-k8s/actions/runs/36189354168)
used SHA `7da932bf9f15c943a8aa025739b5e6706bc95386`. Focused `nodelet`
quick-check [36189351742](https://github.com/centerionware/not-k8s/actions/runs/36189351742),
the five-node Docker isolation preflight, and both nodemigrate/combined-runtime
builds passed. K3s Cilium init containers progressed far enough to start the
agent. Cilium then exhausted ten retries updating
`CiliumNode/runnervmtr4k5`; every retry returned HTTP 409 “object has been
modified”. Logged CRI events for the Cilium Pods all had embedded Pod metadata,
so this run does not prove the missing-metadata fallback was used. Upstream
again imported 55 CRDs then failed CertificateRequest admission because the
destination cert-manager webhook could not be reached while CNI was not ready.
Neither lane reached target workload checks, reverse migration, or parity.
Saved logs: `/tmp/nodemigrate-36189354168/`.

This living list tracks defects exposed while exercising nodemigrate against
the latest regular release (`v0.8.0` at the time of the run), their owning
components, branch fixes, and focused evidence. Release-backed failures caused
by old component binaries remain visible until the fixes are included in the
coordinated `v0.8.1` release.

Branch-runtime integration [36183868918](https://github.com/centerionware/not-k8s/actions/runs/36183868918)
used code SHA `64baa5ab795bfd71fed1ffe193eb08e974e5345d`. It exposed a possible
missed CRI completion event. The nodelet container-ID lookup fallback added
after that run passed focused tests, but the later [36186694756](https://github.com/centerionware/not-k8s/actions/runs/36186694756)
run still captured an API Pod status saying `mount-bpf-fs` was Running after
its log showed bpffs mounted and containerd had no live task. Earlier init
containers did advance to Completed, so this is narrowed to the last observed
transition; whether CRI omitted the event, the lookup failed, or reconciliation
failed is not yet known. Event-path diagnostics are needed before another
behavioral change. Upstream webhook errors remain downstream of CNI failure.
Neither lane reached target workload checks, reverse migration, or parity.
Logs: `/tmp/nodemigrate-36183868918/` and `/tmp/nodemigrate-36186694756/`.

Latest-release integration [36174946764](https://github.com/centerionware/not-k8s/actions/runs/36174946764)
used code SHA `85ed67ed2d4b5540b9cbe3268b061955d946a98b` with regular
`v0.8.0`. The K3s and upstream source fixtures passed; nodemigrate captured
571/58 and 551/54 objects/CRDs. In K3s, the current utility selected
`/etc/cni/net.d` and found Cilium's config, confirming the CNI path correction
is active, but import failed before destination workload checks. Both lanes
reproduced the released target's missing CEL `matches` support for Gateway API
CRDs. Upstream also failed CertificateRequest admission and CSR import with
HTTP 500. Both rollbacks restored their source and retained protected exports;
neither lane reached a target checkpoint or round trip. Logs:
`/tmp/nodemigrate-36174946764/{k3s,kubernetes}.log`.

## Confirmed bugs

| Bug | Owning component(s) | Fix in this branch | Focused evidence / state |
| --- | --- | --- | --- |
| During migration, nodelet requests a second stage path for a volume that kubelet already staged. Nodelet's old path used the raw volume handle under `/var/lib/nodelet/csi`; kubelet uses the SHA-256 handle under `/var/lib/kubelet/plugins/kubernetes.io/csi`. The CSI provider returns `FailedPrecondition: already staged`, leaving workloads Pending. | `nodelet` CSI staging and `nodebootstrap` migration service configuration | Use kubelet-compatible driver/hash/globalmount layout, detect the active source root from mount state, pass it into the generated nodelet service, and expose the preserved staging mount to the replacement CSI Pod. Validate an explicit root against active stages. | Confirmed in [36243049356](https://github.com/centerionware/not-k8s/actions/runs/36243049356). Implemented in `4014685a` and `fc2e5082`; quick-check [36246005553](https://github.com/centerionware/not-k8s/actions/runs/36246005553) passed. Migration run [36245509026](https://github.com/centerionware/not-k8s/actions/runs/36245509026) then confirmed `NodeStageVolume` and `NodePublishVolume` succeeded in both lanes. Cross-provider or different-node CSI data transfer remains unverified. Logs: `/tmp/nodemigrate-36245509026/artifacts/`. |
| Newly created batch/v1 Jobs did not receive Kubernetes-generated selector labels. The Job controller created Pods, but `kubectl logs job/...` selected unrelated Pods. | `nodeapiserver` Job create defaulting for POST and create-on-apply | After assigning request identity and UID, generate upstream legacy and prefixed Job/template labels plus UID-based `spec.selector.matchLabels`; preserve manual selectors. | Confirmed in both lanes of [36245509026](https://github.com/centerionware/not-k8s/actions/runs/36245509026). Fixed in `bd3b4f8f`; focused quick-check [36247102622](https://github.com/centerionware/not-k8s/actions/runs/36247102622) passed. Both lanes of [36247102741](https://github.com/centerionware/not-k8s/actions/runs/36247102741) passed the CronJob log-selection assertion. |
| A migrated PVC can retain `pv.kubernetes.io/bind-completed` and `spec.volumeName` after export strips controller-owned `status`. The PV binder treated the stale marker as complete and never restored `status.phase=Bound`, leaving the static claim Pending. | `nodecontroller` persistent-volume binder; `nodemigrate` status sanitization is the import trigger | Treat the claim as fully bound only when volumeName, bind-completed annotation, and status phase `Bound` are all present. Reconcile incomplete imported claims through the existing prebound-PV path. Add regression coverage for a stale marker without status. | Confirmed in both lanes of [36247102741](https://github.com/centerionware/not-k8s/actions/runs/36247102741). Fixed in `b14185a1`; focused `nodecontroller` quick-check [36248706148](https://github.com/centerionware/not-k8s/actions/runs/36248706148) passed, and migration [36248706224](https://github.com/centerionware/not-k8s/actions/runs/36248706224) observed both imported claims reach `Bound`. |
| Nodelet resolved PVC-backed volumes only when their bound PV had a CSI source. A claim bound to a static `hostPath` or `local` PV stayed Pending indefinitely despite a valid binding. | `nodelet` PVC volume resolution | Resolve bound hostPath and local PV paths directly, validating hostPath types and requiring local PV paths to be directories. Preserve CSI resolution for CSI PVs. Add focused source tests and migration data-read coverage. | Confirmed in both lanes of [36248706224](https://github.com/centerionware/not-k8s/actions/runs/36248706224): PVCs became Bound, then `migration-data-check` timed out because the hostPath PV did not mount. Fix and focused validation are in progress. |
| Import preserved CSI PV metadata and source `volumeHandle`, but the destination CSI driver has no matching provider volume state. `NodeStageVolume` repeatedly returns `NotFound`, so PVC-backed workloads remain Pending; hostPath/local path snapshots do not cover CSI volumes. | `nodemigrate` API transfer and persistent-volume migration; CSI provider lifecycle in the migration fixture | The fixture now backs hostpath CSI `/csi-data-dir` with node-local persistent storage so the provider's volume state and payload survive an in-place driver restart. Still implement or verify provider data movement for replacement onto a different node and for other CSI providers; compare payload at all migration checkpoints. | Confirmed in both lanes of [36241224151](https://github.com/centerionware/not-k8s/actions/runs/36241224151): target hostpath CSI returned `volume id ... does not exist in the volumes list`; source/target PV handles were identical and target node affinity matched. Fixture update is in the branch; `bash -n`/`git diff --check` pass and migration rerun is pending. Logs: `/tmp/nodemigrate-36241224151/`. |
| Server-side apply created imported StatefulSets without `metadata.generation`. `nodecontroller` consequently wrote `status.observedGeneration=null`, and `kubectl rollout status` waited forever for a generation that did not exist. | `nodeapiserver` create-on-apply path | Reuse server-owned generation-1 initialization for create-on-apply and ordinary POST; add a focused regression that verifies client-supplied generation is replaced. | Confirmed in both lanes of [36236810283](https://github.com/centerionware/not-k8s/actions/runs/36236810283): the StatefulSet had generation and observedGeneration null while replica/revision status was populated. Fix and test are in the worktree; nodeapiserver quick-check and migration rerun are pending. |
| The target-state watcher compared `kubectl -o wide` output, including AGE. Normal age increments made every poll look like a state transition and flooded the migration log with repeated pod, log, and event snapshots. | `.github/scripts/nodemigrate-integration.sh` diagnostics | Compare stable normalized pod phase/conditions/container readiness/restarts/IP/node and cert-manager endpoint state. Keep a bounded 30-second diagnostic sample for logs even when state is unchanged. | Confirmed by the many snapshots only seconds apart in both lanes of [36219234465](https://github.com/centerionware/not-k8s/actions/runs/36219234465), including timestamps that advanced while readiness did not. The correction at `35161a3f`, included in run [36220533297](https://github.com/centerionware/not-k8s/actions/runs/36220533297), emitted 10 K3s and 9 upstream snapshots and captured target-time pod and endpoint state; `bash -n` and `git diff --check` passed. This validates bounded diagnostic capture, not the root cause of either migration failure. |
| The ResourceQuota controller compared all `status.used` entries with a map containing only its supported `pods`/`services` keys. If admission supplied `requests.storage`, merge-patching the partial map preserved that unsupported entry, so every ResourceQuota watch update triggered another unchanged status PATCH. The migration fixture produced 6,376–8,366 quota range calls per 30 seconds and over 1,100 quota status PATCHes in about 1.2 seconds. | `nodecontroller` ResourceQuota controller | Compare only the entries this controller owns. Merge-patch its supported entries and clear stale owned keys with explicit nulls, leaving unsupported quota usage intact. Add regression coverage for `requests.storage` and stale supported keys. | Fix at SHA `0e7cf32a` passed focused `nodecontroller` quick-check [36207090965](https://github.com/centerionware/not-k8s/actions/runs/36207090965). Branch-runtime migration [36207264515](https://github.com/centerionware/not-k8s/actions/runs/36207264515) then observed zero `migration-quota/status` PATCHes and only 14–276 K3s / 32–310 upstream ResourceQuota Range calls per 30-second window, versus thousands before. |
| API export omitted every `Endpoints`, `EndpointSlice`, and `Lease` object by kind. This silently lost user-managed external service endpoints and application coordination Leases along with Kubernetes-generated records. | `nodemigrate` API export and semantic snapshot filter | Skip only known controller-generated Endpoints/EndpointSlices, the built-in `default/kubernetes` service endpoints, and kubelet heartbeat Leases in `kube-node-lease`. Preserve other user/add-on-managed endpoint records and Leases. Add fixture objects and assert them at source, target, and return checkpoints. | Rust unit tests passed in focused nodemigrate CI [36199446770](https://github.com/centerionware/not-k8s/actions/runs/36199446770) at SHA `d4f3c4a0`; migration-workflow static validation passed in [36199446771](https://github.com/centerionware/not-k8s/actions/runs/36199446771). Local `bash -n`, snapshot-filter check, and `git diff --check` also pass. Runtime migration validation remains pending. The built-in EndpointSlice ownership labels follow Kubernetes' documented controller ownership convention: [EndpointSlice KEP](https://github.com/kubernetes/enhancements/blob/master/keps/sig-network/0752-endpointslices/README.md). |
| Cilium's agent exhausted ten retries updating its own `CiliumNode` on the destination; each update received HTTP 409 “object has been modified” and the agent exited before CNI readiness. This failure is intermittent and its conflict path remains unconfirmed. | `nodeapiserver` conflict handling or concurrent `CiliumNode` writers; exact writer and request path remain unconfirmed. | Preserve Kubernetes resourceVersion preconditions and storage CAS. Use captured API diagnostics to determine whether conflicts come from stale submitted resourceVersions or storage CAS losses before changing write semantics. | Reproduced in K3s runs [36189354168](https://github.com/centerionware/not-k8s/actions/runs/36189354168), [36192756836](https://github.com/centerionware/not-k8s/actions/runs/36192756836), and [36214776875](https://github.com/centerionware/not-k8s/actions/runs/36214776875). The expanded harness capture observed HTTP 200 updates in runs [36216429427](https://github.com/centerionware/not-k8s/actions/runs/36216429427) and [36217850294](https://github.com/centerionware/not-k8s/actions/runs/36217850294); the 409 did not recur. This is non-reproduction, not a fix or proof of the conflict cause. |
| Both migration lanes fail importing `CertificateRequest/migration-test-1` because the destination cannot reach `webhook.cert-manager.io` (HTTP 500). | Target Cilium/CRI startup, pod networking, and `nodemigrate` import timing; exact cause remains unconfirmed. | Capture target Cilium/CoreDNS/cert-manager Pod state, logs, and webhook service endpoints while the target API is live. Preserve required admission behavior; do not bypass or weaken webhooks. | Reproduced in both lanes of [36217850294](https://github.com/centerionware/not-k8s/actions/runs/36217850294). Run [36220533297](https://github.com/centerionware/not-k8s/actions/runs/36220533297) validates capture during forward migration: upstream snapshots show no webhook Service/EndpointSlice endpoints and cert-manager Pods without phase, container state, or IP; Cilium agent and Envoy became Ready, but all observed CoreDNS pods remained `0/1 Running`. K3s had no webhook endpoints while cert-manager Pods lacked runtime status; at failure capture Cilium agent/Envoy/operator were `1/1`, while CoreDNS remained `0/1 Running`. These are target-time symptoms, not a proven cause. The upstream source API recovered and its protected export was retained; neither lane reached workload, reverse-migration, or parity checks. Logs: `/tmp/nodemigrate-36220533297-k3s.log` and `/tmp/nodemigrate-36220533297-kubernetes.log`. |
| Destination CoreDNS remains `Running` but `0/1 Ready` after Cilium becomes healthy, so the nodelet startup gate leaves ordinary workloads unreconciled. CoreDNS logs report that its Kubernetes plugin is waiting for API synchronization. The target default/kubernetes EndpointSlice still pointed to loopback although the node had a reachable InternalIP. | `nodebootstrap` endpoint handoff; `nodelet` gate correctly waits for CoreDNS readiness | For nodeapiserver with an external CNI, refresh the `default/kubernetes` endpoint to the explicit advertise address or detected non-loopback host IP. Keep the Flannel bridge handoff, and fail with an actionable message if no host address is available. | Run [36229667964](https://github.com/centerionware/not-k8s/actions/runs/36229667964) at SHA `294a6c64` showed the Node InternalIP `10.1.0.61`, Cilium agent/Envoy/operator `1/1`, and CoreDNS `0/1`; the Kubernetes Service's EndpointSlice address was `127.0.0.1:6443`. CoreDNS logs said its Kubernetes plugin was waiting for API synchronization. Inspection found `nodebootstrap` only called its advertise-address refresh for Flannel. The branch now refreshes the target endpoint for external CNI using the explicit address or detected host address; nodebootstrap quick-check and migration runtime verification are pending. Logs: `/tmp/nodemigrate-36229667964/nodemigrate-k3s.log`. |
| The migration failure artifact's fixed 1,000-line API server journal tail can omit the actual CiliumNode conflict window when audit traffic is high. | `.github/scripts/nodemigrate-integration.sh` failure diagnostics | Record each migration's start time and include matching CiliumNode API journal entries from that time in failure output. | Confirmed in K3s run [36214776875](https://github.com/centerionware/not-k8s/actions/runs/36214776875): conflicts occurred 03:44:16–03:44:26Z, while the saved API journal tail began around 03:46:22Z. The matching capture was added and observed CiliumNode HTTP 200 updates in [36216429427](https://github.com/centerionware/not-k8s/actions/runs/36216429427) and [36217850294](https://github.com/centerionware/not-k8s/actions/runs/36217850294); it has not reproduced the conflict. |
| Cilium init-container Pod status can remain at an earlier init phase after its runtime task exits, blocking CNI readiness. | `nodelet` CRI event delivery, CoreDNS-gate reconciliation, CRI state inspection, and Pod-status publication | The branch logs Cilium gate reconciliations, CRI init states, event mapping, and runtime status. Run 36207264515 shows `mount-bpf-fs` start and stop events both resolved with Pod metadata; nodelet reconciled the Pod at init index 3, but status remained `Init:3/6` after the task exited. No following init container started. The exact exit code/reason and status transition remain unresolved; do not classify this as an OOM without kernel/runtime evidence. | The finding recurred in [36207264515](https://github.com/centerionware/not-k8s/actions/runs/36207264515): `mount-bpf-fs` logged that bpffs mounted, emitted a stop event with Pod metadata, and later had no live containerd task while the Pod remained `Init:3/6`. This rules out missing event metadata for this run but does not identify why Pod status failed to advance. The earlier focused nodelet check [36197970777](https://github.com/centerionware/not-k8s/actions/runs/36197970777) passed; focused status-transition verification is still needed. Logs: `/tmp/nodemigrate-36207264515-k3s.log`. |
| The CoreDNS startup gate reconciled local CoreDNS Pods serially before looping back to host-network CNI Pods. When CNI was unavailable, each CoreDNS reconcile timed out after 30 seconds, delaying Cilium init progress by about 90 seconds for three local replicas; the migration attempt expired before the next Cilium pass. | `nodelet` CoreDNS startup gate and keyed Pod reconciler | Enqueue local CoreDNS repairs on the existing bounded, per-Pod coalescing worker pool instead of awaiting them inline. Keep Cilium bootstrap reconciliation responsive while CoreDNS repairs continue asynchronously. | The fix passed focused nodelet quick-check [36209180653](https://github.com/centerionware/not-k8s/actions/runs/36209180653). In migration run [36209180657](https://github.com/centerionware/not-k8s/actions/runs/36209180657), K3s had no 30-second CoreDNS reconcile timeouts; all six Cilium init containers completed and the node became Ready. Logs: `/tmp/nodemigrate-36209180657-k3s.log`. |
| While the CoreDNS startup gate waits for DNS readiness, it reconciled host-network bootstrap Pods and CoreDNS but did not tear down other local Pods already marked for deletion. Their deletions could therefore remain pending for the full gate interval. | `nodelet` CoreDNS startup gate and Pod teardown | During each bounded gate LIST, start the existing keyed/deduplicated teardown for local Pods with a deletion timestamp, while continuing to gate ordinary non-terminating workloads. Add a startup-gate regression covering local versus remote terminating Pods. | K3s runs [36209180657](https://github.com/centerionware/not-k8s/actions/runs/36209180657) and [36211105237](https://github.com/centerionware/not-k8s/actions/runs/36211105237) showed the old failure: CSI Pods remained terminating with `/var/lib/kubelet` paths through the gate. The fix and local-versus-remote regression are in SHA `f5efb03a`; focused nodelet quick-check passed in [36211663128](https://github.com/centerionware/not-k8s/actions/runs/36211663128). Migration run [36212326763](https://github.com/centerionware/not-k8s/actions/runs/36212326763) created fresh replacement CSI Pods using `/var/lib/nodelet`, so the stale-Pod teardown failure no longer reproduced. The CSI Pods then remained Pending on Cilium's agent-not-ready node taint, tracked separately below. Logs: `/tmp/nodemigrate-36209180657-k3s.log`, `/tmp/nodemigrate-36211105237-k3s.log`, and `/tmp/nodemigrate-36212326763-k3s.log`. |
| Cilium agent and operator HTTP probes set `httpGet.host: 127.0.0.1`, but nodelet ignored that field and always connected to the Pod IP. The target Cilium health server bound to `127.0.0.1:9879`; nodelet also dropped Cilium's `brief` and `require-k8s-connectivity` probe headers. | `nodelet` HTTP probe interpretation | Resolve the HTTP probe's explicit host (falling back to Pod IP), send configured HTTP headers, and preserve host-header semantics. | Run [36212326763](https://github.com/centerionware/not-k8s/actions/runs/36212326763) showed agent `ready: false` and repeated operator liveness restarts; source inspection confirmed nodelet connected to the Pod IP and omitted probe headers. The implementation and focused request regressions passed nodelet quick-check [36214612947](https://github.com/centerionware/not-k8s/actions/runs/36214612947). In [36216429427](https://github.com/centerionware/not-k8s/actions/runs/36216429427), Cilium agent, Envoy, and operator were all `1/1 Running` with no restarts. This supports the probe fix at the container readiness boundary, but the complete migration remains unverified. |
| Nodelet ignored Pod- and container-level AppArmor profiles when creating CRI containers. Cilium requested `Unconfined`, but CRI OCI inspection showed `cri-containerd.apparmor.d`; its `mount-cgroup` init container then failed opening `/hostproc/1/ns/cgroup`. | `nodelet` CRI security-context translation | Map Kubernetes `RuntimeDefault`, `Unconfined`, and `Localhost` profiles into CRI `SecurityProfile`, with container-over-pod precedence; retain runtime-default confinement for unexpected profile types. Add regressions for pod-level propagation, override, localhost reference, unset, and unknown values. | Confirmed in [migration run 36162293718](https://github.com/centerionware/not-k8s/actions/runs/36162293718). Fixed in SHA `f4e6fecf`; focused nodelet quick-check passed in [36165304260](https://github.com/centerionware/not-k8s/actions/runs/36165304260). Branch-runtime [36165304330](https://github.com/centerionware/not-k8s/actions/runs/36165304330) showed CRI `profile_type: 1` (`Unconfined`) and `Mounted cgroupv2 filesystem`; later source Cilium port conflicts still prevented the K3s target checkpoint. |
| After source sandbox shutdown, orphaned source Cilium daemons kept host ports and sockets open. The destination agent could not bind Hubble `:4244` and health `:4240`; its operator could not bind metrics `:9963`. Host socket inspection showed both old and destination `cilium-agent` PIDs, and a source `cilium-operator` PID. | `nodemigrate` source Cilium cutover cleanup | Extend exact CRI-container/Pod-UID cleanup from Envoy to the source `cilium-agent` and `cilium-operator` executables. Signal only known Cilium daemons whose cgroup or shim ancestry matches captured source identities; leave all unrelated and destination processes untouched. | Confirmed in K3s lane of [run 36165304330](https://github.com/centerionware/not-k8s/actions/runs/36165304330). Implemented at code SHA `29b6b7de9575e7cf0d43ca60becba868373d86f3`; focused nodemigrate quick-check passed in [36168559894](https://github.com/centerionware/not-k8s/actions/runs/36168559894). Branch-runtime run [36168559234](https://github.com/centerionware/not-k8s/actions/runs/36168559234) confirmed no source Cilium daemon remained and three stale Envoy sockets were removed. The original port collision did not recur; target CNI is now blocked by the separate unresolved initialization failure below. |
| Migration dropped standalone Pod workloads and ReplicaSet/ControllerRevision rollout history by treating all Pods and all revision objects as disposable controller state. The checkpoint filter repeated the same omission, hiding the lost state. | `nodemigrate` | Export standalone Pods and ReplicaSet/ControllerRevision history; skip only controller-owned Pods and static-pod mirrors that are regenerated by their owning controller or kubelet. Align the semantic snapshot filter and add a standalone Pod and StatefulSet history to the round-trip fixture. | Confirmed by `SKIP_KINDS` and the matching snapshot normalizer. Focused exporter tests and real source → nodestore → source checks are being added; targeted nodemigrate CI and branch-runtime integration are pending. |
| Source objects can use an API version that the destination no longer serves even though discovery exposes another version of the same group and kind. ClusterTrustBundle import failed against `v0.8.0`. | `nodemigrate` | Negotiate the discovered destination version only for the same API group and kind; retain source version otherwise and report selected conversion. | Focused nodemigrate checks passed at SHA `954f1be3d12fb7f0334db8d8ff6efae28ca0e4250` ([run 36068373192](https://github.com/centerionware/not-k8s/actions/runs/36068373192)). Release integrations `36068417485` and `36071174265` confirmed ClusterTrustBundle/v1 was applied through v1beta1. |
| Gateway API v1.6.1's `safe-upgrades` ValidatingAdmissionPolicy uses the global CEL function `matches(string, regex)`. nodeapiserver did not register that function, so three Gateway CRDs were rejected during migration with `Undeclared reference to 'matches'`. | `nodeapiserver` CEL runtime | Register the global `matches` function using the existing bounded regex path and add a focused regression with the exact policy expression shape. | Reproduced in both branch-runtime lanes of [run 36142617576](https://github.com/centerionware/not-k8s/actions/runs/36142617576). The fix at SHA `c56d3f3ecce3a43339ba0df23aec02a36193d841` passed the `nodeapiserver` quick-check ([run 36145507191](https://github.com/centerionware/not-k8s/actions/runs/36145507191)); both lanes in [run 36145520889](https://github.com/centerionware/not-k8s/actions/runs/36145520889) passed this CEL function and then exposed the separate map-comprehension issue below. In latest-release run [36153673997](https://github.com/centerionware/not-k8s/actions/runs/36153673997), both source fixtures passed but the `v0.8.0` target again rejected Gateway CRDs with `Undeclared reference to 'matches'`; this confirms the release baseline defect remains until the branch server fix ships in `v0.8.1`. The upstream CertificateRequest HTTP 500 remains independently tracked. |
| Gateway API v1.6.1 CRDs use bounded map validations such as `self.all(key, key.matches(...))`. The schema checker treated typed `additionalProperties` objects as generic objects, rejected map comprehensions, and the cost estimator treated map keys as unbounded. Three CRDs failed import despite valid source-side creation. | `nodeapiserver` CEL type checker and cost estimator | Model object schemas with `additionalProperties` and no named fields as CEL maps; resolve single-variable map comprehensions to the map's key path so key regex cost uses the schema's bounded map cardinality and CEL's unbounded-by-schema string-key size. Add focused type and cost regressions using the Gateway API rule shape. | Confirmed in both branch-runtime lanes of [run 36145520889](https://github.com/centerionware/not-k8s/actions/runs/36145520889), exact SHA `c56d3f3ecce3a43339ba0df23aec02a36193d841`. The source fixture passed in both lanes; migration then failed on the three Gateway CRDs with type errors (`comprehension requires list or map range, got object`) and saturated cost estimates. The implementation is at `6a2e99d6b205626ae125e231d276c1936c3f1986`; its first quick-check [36148371547](https://github.com/centerionware/not-k8s/actions/runs/36148371547) caught a test-only path error. That was corrected at `8555845f`, and the focused nodeapiserver quick-check passed ([run 36148848411](https://github.com/centerionware/not-k8s/actions/runs/36148848411)). In [run 36150670405](https://github.com/centerionware/not-k8s/actions/runs/36150670405), every Gateway CRD, including TLSRoute, was accepted in both lanes; the remaining target Cilium/CNI and upstream webhook issues are tracked separately. |
| Gateway API TLSRoute hostname validation uses `hostname.substring(2).matches(...)`. CEL cost estimation could not derive a result bound for `substring`, so valid bounded hostname rules saturated the CRD static-cost estimate and TLSRoute CRDs were rejected. | `nodeapiserver` CEL cost estimator | For `substring`, propagate the source string's maximum size to the result and use zero as the conservative minimum. Add a regression with the actual Gateway-style bounded hostname list, length, substring, and regex combination. | Confirmed in [run 36148384987](https://github.com/centerionware/not-k8s/actions/runs/36148384987), exact source SHA `6a2e99d6b205626ae125e231d276c1936c3f1986`. Fix at `f97432ccc3edc65c846cb4c3c7bc7da3e30487f9` passed focused nodeapiserver checks in [run 36150653684](https://github.com/centerionware/not-k8s/actions/runs/36150653684), and [run 36150670405](https://github.com/centerionware/not-k8s/actions/runs/36150670405) accepted all source CRDs in both lanes. |
| Protobuf encoding of Kubernetes `ExtraValue` arrays treated them as objects, so importing a CertificateSigningRequest returned HTTP 500. | `nodeapiserver` | Encode authentication, authorization, and certificates `ExtraValue` values as their repeated-string protobuf representation; regression tests cover encode/decode. | Quick-check for `nodeapiserver,nodecontroller,nodebootstrap` passed at fix SHA `f9e4b31139a30fb7a453161e4dc2295983fcef8f` ([run 36071174298](https://github.com/centerionware/not-k8s/actions/runs/36071174298)). `v0.8.0` reproduced the server error in runs `36071174265` and `36102899864`; the current branch-runtime run `36102292369` did not report a CSR failure. Fix must ship in `v0.8.1`. |
| Controller-specific writes still arrived as `system:kube-controller-manager`: the not-k8s API server authenticated the base certificate but ignored `Impersonate-User` and `Impersonate-Group`, so migrated workloads got HTTP 403 creating ReplicaSets. | `nodeapiserver`, `nodecontroller`, `nodebootstrap` | API server now checks the original caller's RBAC permission to impersonate each ServiceAccount and group, then authorizes the request as that effective identity. Unsupported UID/extra impersonation headers are rejected. Controller reads stay on the base identity and writes use their narrow controller service accounts. | Confirmed in branch-runtime integration run `36078155500` at SHA `3ef3ddf1e544a4ba7eda328c1b841ccd51aee9e1`; both source lanes hit 403 creating ReplicaSets. The server-side RBAC-checked impersonation fix and parser regression tests are committed at `d4847449a84a5014515f31b3d9d522e455910cf7`; focused quick-check for `nodeapiserver,nodemigrate` passed ([run 36080595243](https://github.com/centerionware/not-k8s/actions/runs/36080595243)). In branch-runtime run [36080871524](https://github.com/centerionware/not-k8s/actions/runs/36080871524), both lanes reached destination API readiness, but no controller ReplicaSet/Pod create or delete requests appeared in the captured API audit before the next failure. Effective controller-write recovery remains unverified. Earlier component quick-check at `f9e4b31139a30fb7a453161e4dc2295983fcef8f` ([run 36071174298](https://github.com/centerionware/not-k8s/actions/runs/36071174298)) did not cover server-side impersonation. |
| Cilium-based startup deadlocked behind the CoreDNS readiness gate: nodelet paused its Pod watch until CoreDNS was healthy, but Cilium's host-network agent Pods were also waiting for nodelet reconciliation. Containerd then reported `cni plugin not initialized`, leaving CoreDNS and all ordinary workloads unable to start. | `nodelet` | During the CoreDNS gate, list Pods assigned to this node and reconcile host-network Pods. These bypass CNI and allow external CNI agents such as Cilium to start; ordinary Pods remain gated. | Confirmed in both lanes of branch-runtime run [36082829012](https://github.com/centerionware/not-k8s/actions/runs/36082829012) at SHA `fc7acb76e84372fe0c586fdc4d48012cfad1f133`: nodelet logged `waiting for CoreDNS ...`, CoreDNS stayed unready, Cilium Pods remained `Unknown`, and containerd repeatedly logged `cni plugin not initialized`. The local-host-network selector test and implementation pass nodelet quick-check [36084558060](https://github.com/centerionware/not-k8s/actions/runs/36084558060) at SHA `c94e109c9b49c778c5dae50bccf880a58a4d6e23`. Migration run [36084558323](https://github.com/centerionware/not-k8s/actions/runs/36084558323) shows host-network Cilium sandboxes and init containers now start, but Cilium/CNI still does not become ready because of the separate read-only mount failure below. |
| Cilium Envoy cannot mount writable `sockets` and `artifacts` directories beneath its read-only `/var/run/cilium/envoy/` volume. runc tries to create those targets after mounting the parent read-only volume and gets `read-only file system`. | `nodelet` CRI volume preparation | Before sending the CRI request, create nested mount targets inside read-only parent sources only when those sources are nodelet-managed Pod volumes. Leave the requested RO/RW flags intact and never mutate arbitrary hostPath or CSI data. | Reproduced in both lanes of [run 36088034808](https://github.com/centerionware/not-k8s/actions/runs/36088034808) and [36089689047](https://github.com/centerionware/not-k8s/actions/runs/36089689047). Envoy's own OCI spec in [run 36091505933](https://github.com/centerionware/not-k8s/actions/runs/36091505933) shows readonly `/var/run/cilium/envoy/` preceding child mounts at `sockets` and `artifacts`; its rootfs itself is not marked read-only. The managed-volume preparation and safeguards are committed at `f048ae142d8dbfe6ee9fa28e6a7a2a8c4a51702c`; nodelet quick-check passed in [run 36093508613](https://github.com/centerionware/not-k8s/actions/runs/36093508613). In branch-runtime run [36093516629](https://github.com/centerionware/not-k8s/actions/runs/36093516629), the prior runc mountpoint error did not recur in either lane, but Envoy then failed its startup probe and CNI remained unready. Container logs are being added to the next failure bundle, so the full Cilium startup fix and migration gate remain unverified. |
| K3s pod cleanup ran after `systemctl stop k3s`, which also shuts down K3s's embedded containerd; CRI requests to the correct socket then returned connection refused. The earlier generic fallback also selected the wrong runtime. | `nodemigrate` cutover orchestration and source runtime detection | Detect a configured `container-runtime-endpoint` or use K3s's embedded socket, then stop K3s sandboxes while the K3s service/runtime is still active and only then disable the service. Keep kubeadm cleanup after kubelet stop because its containerd service is separate. | Reproduced in branch-runtime [run 36102292369](https://github.com/centerionware/not-k8s/actions/runs/36102292369) and latest-release [run 36102899864](https://github.com/centerionware/not-k8s/actions/runs/36102899864): `crictl` reached `unix:///run/k3s/containerd/containerd.sock` after the K3s stop and got `connection refused`. The worktree now moves K3s sandbox cleanup before service shutdown; focused quick-check and runtime retest are pending. |
| Cilium Envoy can remain alive after its source CRI sandbox stops, keeping a host-wide socket bound and blocking target Cilium startup. Envoy may run inside the source `cilium-agent` container, and its cgroup ID may differ from the owning containerd shim's `-id`. | `nodemigrate` source sandbox cutover and `nodelet` Cilium startup | Record IDs for source Cilium agent and Envoy containers. After source shutdown, stop only Envoy executables whose cgroup ID or exact ancestor containerd-shim `-id` matches one of those source CRI IDs. Send SIGTERM, wait, then SIGKILL only still-matching processes; remove stale socket paths afterward. Abort cutover and restore the source service if cleanup fails. Never kill by process name alone. | Process diagnostics in [run 36107163563](https://github.com/centerionware/not-k8s/actions/runs/36107163563) confirmed the source K3s owner. Latest implementation at SHA `aa4580f0938fb8c23b816c41b61d5e96c8314e4a` stopped both stale Envoy processes and removed stale sockets in branch-runtime run [36112039343](https://github.com/centerionware/not-k8s/actions/runs/36112039343) and latest-release `v0.8.0` run [36112039214](https://github.com/centerionware/not-k8s/actions/runs/36112039214). The original source socket collision did not recur. At [run 36121960626](https://github.com/centerionware/not-k8s/actions/runs/36121960626), source cleanup found no Envoy process matching the two tracked source CRI IDs and removed three stale sockets. After target import/readiness, destination Cilium's Envoy Pod was `CrashLoopBackOff` with `errno=98`; its Pod UID was `d5b4f90a-acc1-4483-9e02-5823e7b22f4a`, its current container cgroup named CRI ID `464c40568a359403a583cc37b8040a431e6a39c292b704e648e6a0fc289a4c2c`, but its process ancestor shim used ID `3e95e25fd690942e9b43153b32014a4978a66bff3d8a050988854072aefa13ad` and `/run/k3s/containerd/containerd.sock`. The ownership cause remained unresolved. In [run 36150670405](https://github.com/centerionware/not-k8s/actions/runs/36150670405), cleanup again found no process matching the source CRI IDs and removed three stale sockets; target Envoy then failed binding with `errno=98`. Host PID 32311/32344 belonged to cgroup CRI ID `e14332ccaab8e20bdecfcff6bcc7140104799be59028628b3ebb15bcbd17e78a`, under `k3s.service` shim ID `e512be6d65204325544d6ea9e454bd4f94e12617cde8e12bb630354239d7cf9f`; neither matched the two recorded source IDs. CNI did not initialize, leaving CoreDNS and hostPath CSI unable to run. These process and shim identities still require tracing before any cleanup change; never kill by process name alone. |
| Applying `cert-manager.io/v1/CertificateRequest migration-test-1` returned HTTP 500 because the destination could not call the `webhook.cert-manager.io` mutating webhook at `cert-manager-webhook.cert-manager.svc/mutate`. The webhook Pod was unavailable while target Cilium networking failed. | `nodelet` Cilium/CRI startup and `nodemigrate` restore ordering; the API server correctly rejected a required webhook call | Target migration diagnostics now save dedicated API server and datastore journals. Fix the target Cilium networking/readiness failure, then ensure webhook-backed objects are restored only when their admission service can answer without skipping or weakening the webhook. | Branch-runtime [run 36102292369](https://github.com/centerionware/not-k8s/actions/runs/36102292369) and v0.8.0 [run 36102899864](https://github.com/centerionware/not-k8s/actions/runs/36102899864) both reproduced the 500. The v0.8.0 API journal explicitly logged `admission webhook invocation failed` for the CertificateRequest; containerd also logged the Cilium CNI plugin exiting with `signal: killed`, and cert-manager Pods were `Error`. The failure recurred in [run 36150670405](https://github.com/centerionware/not-k8s/actions/runs/36150670405) after all 54 upstream CRDs were accepted: CertificateRequest admission could not reach cert-manager while destination CNI was unavailable. Latest-release run [36153673997](https://github.com/centerionware/not-k8s/actions/runs/36153673997) again returned 500 for CertificateRequest and CSR objects; CertificateRequest admission depends on target webhook networking and CSR may also exercise the separately tracked v0.8.0 `ExtraValue` codec defect. Run [36217850294](https://github.com/centerionware/not-k8s/actions/runs/36217850294) reproduced the webhook 500 in both lanes. Its CoreDNS and Cilium pod details were captured after source rollback, so they do not describe target state. The target-state watcher added afterward is awaiting runtime validation. Protected exports were retained; the target networking cause and a complete migration retry remain unresolved. |
| Removing a stopped upstream CRI pod sandbox can time out in containerd during CNI teardown, which previously aborted migration and restored the source service even though the sandbox's containers had stopped. | `nodemigrate` CRI cutover | Stop ordinary sandboxes before Cilium, verify no containers remain running, and tolerate a sandbox-removal error only when CRI confirms that sandbox has no running containers. Keep rollback when any source process is still active. | Reproduced in upstream [run 36096822314](https://github.com/centerionware/not-k8s/actions/runs/36096822314), where `crictl rmp` returned `DeadlineExceeded`. The follow-up passed nodemigrate quick-check [36098539568](https://github.com/centerionware/not-k8s/actions/runs/36098539568); integration [36098546161](https://github.com/centerionware/not-k8s/actions/runs/36098546161) advanced past sandbox cleanup to object import, so this specific failure did not recur. |
| The upstream import failure summary hid the underlying API response needed to diagnose the rejected object. | `nodemigrate` import diagnostics | Preserve the full `anyhow` error chain in the retry summary. | Reproduced in [run 36098546161](https://github.com/centerionware/not-k8s/actions/runs/36098546161). Fixed in the worktree; quick-check passed in [run 36100274964](https://github.com/centerionware/not-k8s/actions/runs/36100274964), and [run 36100281843](https://github.com/centerionware/not-k8s/actions/runs/36100281843) now reports the exact CertificateRequest route and HTTP 500. |
| Earlier Cilium failure bundles omitted `mount-cgroup` output when nodelet removed the exited init container before final CRI inspection. | `nodelet` init-container lifecycle diagnostics and migration harness | Correlate nodelet create/start/retry logs by Pod/container/CRI ID and watch the Cilium `mount-cgroup` container during target CSI setup so its logs are saved before cleanup. | Exact exit IDs were confirmed in [run 36159532331](https://github.com/centerionware/not-k8s/actions/runs/36159532331). The watcher was exercised in branch-runtime [run 36165304330](https://github.com/centerionware/not-k8s/actions/runs/36165304330) and captured `Mounted cgroupv2 filesystem` after the AppArmor fix. Cilium later failed on source host-port conflicts, tracked separately above. |
| After forward cutover, the migration harness left `CURRENT_KUBECONFIG` pointing at the stopped source while reinstalling CSI and collecting failure diagnostics. This made the Cilium query stale and produced unknown-CA errors against the destination, obscuring target state. | `.github/scripts/nodemigrate-integration.sh` (migration test harness) | Switch the active kubeconfig to the nodestore destination immediately after successful forward migration, before CSI redeployment and any target-stage diagnostics. | Confirmed in branch-runtime [run 36119449016](https://github.com/centerionware/not-k8s/actions/runs/36119449016): the K3s migration imported all 48 CRDs and passed destination API readiness, then target CSI setup failed. The active-kubeconfig handoff is in the test harness at SHA `d739633a15a330565339b7028e9339e73326b207`. In [run 36121960626](https://github.com/centerionware/not-k8s/actions/runs/36121960626), diagnostics used the destination and exposed its Cilium Envoy `errno=98` failure plus the CRI ownership mismatch tracked above. The selected Cilium Pod name can disappear between listing and fetching logs, so the log helper still needs retry/current-UID lookup; migration fixture and CSI checks did not pass.
| If target bootstrap/readiness/API import failed after source shutdown, nodemigrate left the source disabled and the target stack partially active, causing service/port conflict and no automatic recovery. The upstream `v0.8.0` CSR failure reproduced this path. | `nodemigrate` | Stop any partially installed nodestore services, restore the source's recorded service state, retain the protected export, and report both original failure and recovery outcome. Integration script checks restored source service/API and retained export after a failed migration. | Nodemigrate crate tests passed at SHA `f9e4b31139a30fb7a453161e4dc2295983fcef8f` ([run 36071161354](https://github.com/centerionware/not-k8s/actions/runs/36071161354)). Release-backed assertion passed in run `36071174265`, attempt 2: the original CSR import failed, kubelet and source API recovered, and the export remained present. |
| Five-node Docker preflight falsely rejected a healthy containerd CRI plugin because `ctr plugins ls` pads its fixed-width output with trailing spaces. | `.github/scripts/nodemigrate-docker-preflight.sh` (migration CI harness) | Match plugin type, ID, and final status as parsed fields instead of requiring unpadded end-of-line output. | Confirmed in run `36074502550`; fixed parser passed in `36075750885`, which proceeded to the BPF compile probe. |
| Traefik GatewayClass `/status` updates received HTTP 409 after forward migration in both source lanes, leaving `Accepted=Unknown`. | `nodeapiserver` streaming-list WATCH initial events | Stamp synthetic initial ADDED objects with their own `mod_revision`; retain the snapshot revision only for the initial-events-end bookmark. Add a regression with multiple object revisions. | Root cause confirmed in [run 36234240173](https://github.com/centerionware/not-k8s/actions/runs/36234240173): Traefik submitted RV 1382 vs stored 1049 in K3s and RV 891 vs stored 889 upstream. The per-object initial-event fix and regression are in the branch; nodeapiserver quick-check and migration rerun are pending. |
| The new allowed-NetworkPolicy probe correctly fetched and matched the nginx page with `wget | grep -q`, but then incorrectly required that page body in Job logs; `grep -q` deliberately emits no matching text. Both K3s and upstream source lanes stopped at this harness assertion before nodemigrate ran. | `.github/scripts/nodemigrate-integration.sh` migration fixture | Treat successful Job completion as the assertion: the shell exits zero only if wget receives the page and grep finds the expected nginx text. Remove the contradictory log-body check. | Confirmed in both lanes of [run 36179275793](https://github.com/centerionware/not-k8s/actions/runs/36179275793): source workloads and NetworkPolicy creation succeeded, the allowed probe Job reached Complete, then the log grep failed because quiet grep wrote no page text. Fix is in this branch; migration-stage fixture validation is pending a rerun. |
| The Docker BPF probe did not compile because its global symbol was named `license`, which clang's BPF assembler rejected as a duplicate symbol. | `.github/nodemigrate/bpf-probe.c` (migration CI harness) | Use the conventional `LICENSE` symbol name while retaining the required ELF `license` section. | Confirmed in run `36075750885`; the renamed source compiled and loaded successfully in Docker-only run `36077448685`. |
| The five-node image resolved Ubuntu's `bpftool` wrapper, which searched for tools matching the runner's Azure kernel version and failed because that versioned binary was absent in the container. | `.github/nodemigrate/five-node.Dockerfile` (migration CI harness) | Install the generic versioned Linux tools package and point `/usr/local/bin/bpftool` directly at the packaged executable, avoiding the host-kernel-version wrapper lookup. | Confirmed in run `36077006621`; a first path assumption failed image build in `36077228915`. The widened package search then built and successfully ran the BPF load/show checks in Docker-only run `36077448685`. |

| Stopping a ready K3s source pod sandbox can time out with CRI `DeadlineExceeded` and abort before cutover, even when containerd subsequently reports no running containers for that sandbox. | `nodemigrate` CRI cutover | Check the CRI container list after stop attempts. Tolerate an individual stop error only when that sandbox has no running containers; retain rollback if any failed-stop sandbox is still running. Add a focused decision test. | Confirmed in K3s lane of [run 36231779729](https://github.com/centerionware/not-k8s/actions/runs/36231779729). The guarded behavior and unit regression are in the branch; targeted nodemigrate CI and runtime rerun are pending. |

## Test infrastructure issue

| Issue | Owner | State |
| --- | --- | --- |
| Post-migration verification used kubectl's `crd` shortcut, which is not advertised by the destination API; the upstream lane therefore failed before the semantic checkpoint even though forward migration and API readiness completed. | `.github/scripts/nodemigrate-integration.sh` | Replaced `get crd` with `get customresourcedefinitions.apiextensions.k8s.io` in stage checks and checkpoint capture. Confirmed as the failure in [run 36231779729](https://github.com/centerionware/not-k8s/actions/runs/36231779729); rerun pending. |
| The source-stage CA assertion decoded kubeconfig PEM into shell command substitution, which strips trailing newlines and falsely reported all namespace bundles mismatched in both lanes. | `.github/scripts/nodemigrate-integration.sh` migration fixture | Compare exact CA bytes by base64-encoding ConfigMap data and comparing it with kubeconfig `certificate-authority-data`. Fixed and exercised successfully in both source stages and repeated target snapshots in [run 36226000144](https://github.com/centerionware/not-k8s/actions/runs/36226000144). |
| Both source lanes timed out waiting for `Gateway/migration-traefik` to become Programmed before nodemigrate started. `GatewayClass` was Accepted, Cilium and CSI were healthy, and Traefik was Running at failure capture; the harness had not captured Gateway status or Traefik logs. | `.github/scripts/nodemigrate-integration.sh` (migration fixture) | Added a Traefik rollout barrier before creating Gateway API fixtures and added Gateway/HTTPRoute status plus Traefik logs to failure diagnostics at SHA `404d68f7b3aa6dc04192e2061e48f563ae2d5589`. This tests a possible setup-order cause without claiming it is confirmed. Runtime rerun pending. Reproduced in [run 36124385324](https://github.com/centerionware/not-k8s/actions/runs/36124385324); full logs: `/tmp/nodemigrate-36124385324/`. |
| The K3s fixture attempted to resolve the source node InternalIP immediately after `k3s` started. The API was reachable before the first Node object registered, so JSONPath indexed an empty list and the lane exited before Cilium installation. | `.github/scripts/nodemigrate-integration.sh` (migration CI harness) | Wait up to three minutes for the first registered node InternalIP before configuring Cilium; this does not wait for Node Ready because the Cilium CNI must make it Ready. | Reproduced in [run 36086342991](https://github.com/centerionware/not-k8s/actions/runs/36086342991). The bounded wait passed in [run 36088034808](https://github.com/centerionware/not-k8s/actions/runs/36088034808): K3s registered its first node, source Cilium and workloads passed, and forward migration completed. Target Cilium readiness remains blocked by the separate nodelet mount-order issue. |
| After forward migration, the hostpath CSI StatefulSet Pods `csi-hostpath-socat-0` and `csi-hostpathplugin-0` remained `Terminating` for over five minutes, preventing post-cutover storage checks and reverse migration. | `nodelet` | The pod deletion events could not be handled while the CoreDNS gate prevented the regular Pod watch from starting. Reconcile local host-network bootstrap Pods during the gate so Cilium can initialize networking, CoreDNS can become ready, and normal teardown/reconciliation can proceed. | Run [36082829012](https://github.com/centerionware/not-k8s/actions/runs/36082829012) first reproduced this in both source lanes. Fix SHA `c94e109c9b49c778c5dae50bccf880a58a4d6e23` passed nodelet quick-check but migration rerun [36084558323](https://github.com/centerionware/not-k8s/actions/runs/36084558323) still failed: host-network Cilium Pods began starting, but CNI remained unready due to the read-only mountpoint failure above. CSI cleanup and all later verification remain blocked pending that runtime fix. Failure diagnostics now include `crictl pods/ps`, Cilium Pod YAML, nodelet journal, and cluster events. |
| Five privileged Docker node containers initially exited with code 255 before systemd readiness, preventing the required three-control-plane/two-worker isolation lane from running. | `.github/nodemigrate` image and Docker preflight | Keep each container's private cgroup namespace without shadowing it with the host cgroup bind mount; set systemd `/run` tmpfs mounts to mode 0755. Add specific checks for CRI, namespace, BTF, bpffs, BPF load, persistent storage, networking, and restart isolation. | The full Docker isolation preflight passed in [run 36077448685](https://github.com/centerionware/not-k8s/actions/runs/36077448685): five containers had distinct network/mount namespaces and machine IDs, healthy CRI and loaded BPF, independent persistent volumes, inter-node reachability, and single-node stop/restart isolation. This does not establish Kubernetes, Cilium datapath, or migration parity; those remain unverified in the actual five-node migration lane. |
| Gateway fixture used listener port 80 although the installed Traefik Gateway API controller advertises its supported listener on port 8080; the resulting Gateway was rejected as `PortUnavailable`. | `.github/scripts/nodemigrate-integration.sh` fixture | Bind the test Gateway to port 8080 and retain Gateway/HTTPRoute status checks. | Confirmed from [run 36126850738](https://github.com/centerionware/not-k8s/actions/runs/36126850738), where Gateway status reported `Cannot find entryPoint ... port 80`. The correction was confirmed by [run 36129138298](https://github.com/centerionware/not-k8s/actions/runs/36129138298), where the fixture Gateway became Programmed; the same run exposed a separate nested HTTPRoute wait bug below. |
| Expanded source fixtures timed out waiting for the persistent-volume seed Pod. Upstream Kubernetes eventually reported `Insufficient cpu` for the fixture Pods on its single-node source; the failure diagnostics did not capture node allocation or the blocked seed details. | `.github/scripts/nodemigrate-integration.sh` fixture | Add small explicit CPU/memory requests to fixture containers and dump node allocation and seed/standalone/nginx Pod descriptions on failure. | Confirmed in [run 36126850738](https://github.com/centerionware/not-k8s/actions/runs/36126850738). The next run [36129138298](https://github.com/centerionware/not-k8s/actions/runs/36129138298) confirmed the 1m/1Mi requests let the seed Pod and all fixture Pods schedule; no migration ran because of the separate route-wait bug below. |
| The fixture used `kubectl wait --for=condition=Accepted/ResolvedRefs` for HTTPRoute status, but Gateway API stores those conditions under `.status.parents[].conditions[]`; both routes were accepted and references resolved while kubectl wait timed out. | `.github/scripts/nodemigrate-integration.sh` fixture | Poll the nested HTTPRoute parent conditions and print the route status on timeout. | Confirmed by [run 36129138298](https://github.com/centerionware/not-k8s/actions/runs/36129138298): upstream status showed `Accepted=True` and `ResolvedRefs=True` while kubectl wait timed out. In [run 36130714079](https://github.com/centerionware/not-k8s/actions/runs/36130714079), both lanes passed the new parent-condition polling and proceeded to later source assertions. |
| Source fixture validation referenced `$stage` under `set -u` before the function initialized it, aborting after the Gateway and HTTPRoute checks. | `.github/scripts/nodemigrate-integration.sh` fixture | Set the fixture verification stage to `source` in `install_workloads`. | Confirmed by [run 36130714079](https://github.com/centerionware/not-k8s/actions/runs/36130714079): both K3s and kubeadm lanes failed at the PriorityClass assertion with `stage: unbound variable`; Gateway and HTTPRoute setup had already passed. Fix is in the worktree; rerun pending. |
| PriorityClass fixture asserted `globalDefault` was serialized as explicit `false`, but Kubernetes omits the default false field from the API response. | `.github/scripts/nodemigrate-integration.sh` fixture | Treat an omitted `globalDefault` as its Kubernetes default value `false` in the semantic assertion. | Confirmed by [run 36131837328](https://github.com/centerionware/not-k8s/actions/runs/36131837328), where both lanes reached the source PriorityClass assertion and failed despite creation succeeding. API-default-aware assertion is in the worktree; rerun pending. |
| Secret fixture used jq dot notation for the hyphenated key `migration-secret`, which jq parsed as subtraction and rejected as a syntax error. | `.github/scripts/nodemigrate-integration.sh` fixture | Select the value with `.data["migration-secret"]`. | Confirmed in both lanes of [run 36133100159](https://github.com/centerionware/not-k8s/actions/runs/36133100159), after source fixtures passed. The bracket-notation correction is in the worktree; rerun pending. |
| RBAC Node-read Job manifest was emitted from an expanding shell heredoc, which executed `$(NODE_NAME)` on the runner instead of preserving it for expansion by Kubernetes in the Job container. | `.github/scripts/nodemigrate-integration.sh` fixture | Escape the command substitution in the heredoc so Kubernetes expands the container's downward API environment variable. | Confirmed in both lanes of [run 36134496060](https://github.com/centerionware/not-k8s/actions/runs/36134496060): the fixture printed `NODE_NAME: command not found` and the authorized ConfigMap Job timed out. The next run passed that point; RBAC Pods then remained Pending for CPU capacity, tracked separately below. |
| RBAC Jobs and the later PV data-check Pod did not declare small resource requests; their source Pods remained Pending with `Insufficient cpu` on the single-node fixture. | `.github/scripts/nodemigrate-integration.sh` fixture | Set 1m CPU/1Mi memory requests on all helper probe/data-check containers. | Confirmed in [run 36136419993](https://github.com/centerionware/not-k8s/actions/runs/36136419993) on both K3s and upstream Kubernetes. The three RBAC Pods were all Pending with scheduler events reporting `Insufficient cpu`; the workload fixtures and earlier Jobs had succeeded. At [run 36138657733](https://github.com/centerionware/not-k8s/actions/runs/36138657733), allowed ConfigMap/Node reads and expected denied Secret reads completed on both source clusters. Migration-stage RBAC remains unverified. |
| The Gateway API source probe sent `migration-gateway.test` to Traefik Service port 80, which forwards to EntryPoint 8000; the Gateway listener was configured on EntryPoint 8080, so it returned 404 even though the route was accepted and programmed. | `.github/scripts/nodemigrate-integration.sh` fixture | Keep the Ingress probe on Service port 80 and port-forward the Gateway probe directly to Traefik Pod port 8080. Retain response and endpoint diagnostics on failure. | Confirmed in both lanes of [run 36140783446](https://github.com/centerionware/not-k8s/actions/runs/36140783446): the Ingress returned HTTP 200 with nginx and Gateway returned HTTP 404; Service EndpointSlices targeted 8000/8443. The test harness now probes the configured Gateway listener at 8080. Runtime recheck pending. |
| A branch-built destination can leave a successful Cilium init container reported Running, so later init containers and the Cilium agent never start and the node remains CNI-unready. | `nodelet` CRI event-to-Pod reconciliation; exact event payload is unverified | Resolve CRI events by container ID and runtime labels when `PodSandboxStatus` is absent; add regressions for event payloads with and without sandbox metadata. | Run [36183868918](https://github.com/centerionware/not-k8s/actions/runs/36183868918) at SHA `64baa5ab795bfd71fed1ffe193eb08e974e5345d` showed `apply-sysctl-overwrites` exited with code 0 in CRI at 20:26:53Z while Pod status still reported it Running at 20:28:17Z; `mount-bpf-fs` was never created. No nodelet Pod-status patch appears after the init start in the captured interval. This supports a lost/missed reconciliation event, but the actual CRI event payload was not captured. A container-ID fallback and focused nodelet regression are in the worktree; validation pending. The upstream CertificateRequest webhook failure is downstream of the unavailable destination CNI. Logs: `/tmp/nodemigrate-36183868918/nodemigrate-k3s-36183868918/nodemigrate-k3s.log` and `/tmp/nodemigrate-36183868918/nodemigrate-kubernetes-36183868918/nodemigrate-kubernetes.log`. |

## Release target

The intended first nodemigrate release is coordinated with regular release
`v0.8.1`. Nodemigrate must not itself bump the shared version or create a
different regular release number. The component fixes above must be included
in the `v0.8.1` runtime and the standalone utility artifact must carry that
same version. This tracker does not authorize publication or merging.

See the [CI run record](NODEMIGRATE_CI_STATUS.md), [migration status](NODEMIGRATE_MIGRATION_STATUS.md), and [release status](NODEMIGRATE_RELEASE_STATUS.md).
