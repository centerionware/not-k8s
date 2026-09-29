# nodemigrate implementation and integration status

Last updated: 2026-09-29

This is the living implementation status record for the full scope in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). Capability marks describe code
that exists; verification marks describe evidence from a run. A passing
compile or unit test does not mark a real migration path as verified.

The source cutover exports API objects while the source API is live, then stops
the source service/runtime before copying node-local PV data. Ordinary Pod
sandboxes are retained; explicit removal is limited to Cilium sandboxes and
kubeadm control-plane static Pods needed to release API/etcd ports. Upstream
runtime service state is captured so a failed snapshot/import can stop the
partial destination, restore saved PV/CNI payloads, and restart the original
runtime and source services. Late failures during node join, readiness, and
scheduling use the same rollback path. Target rollback removes only identified
Cilium sandboxes/processes and kubeadm API/etcd static Pods; ordinary Pod
sandboxes remain on disk. The implementation passed nodemigrate-only
[quick-check run 36522042632](https://github.com/centerionware/not-k8s/actions/runs/36522042632)
at `6c55fba88c47a7e1f0a5e5ef565cdec9cdfed825`. The real migration paths remain
unverified; the known post-return Cilium routing failure still blocks a rerun.

The stage fixture now mints a short-lived ServiceAccount token and checks the
TokenReview identity plus allowed and denied SubjectAccessReview decisions at
each source, not-k8s, and returned-source checkpoint. It also exercises a
versioned custom resource's `/status` update and readback at each checkpoint.
Shell syntax and focused JSON filter checks pass; these new migration fixture
assertions have not run against a cluster yet, and the known Cilium routing
failure still blocks a migration rerun.

The importer now preserves standalone Pod ephemeral-container specifications
by applying them after Pod creation through `pods/ephemeralcontainers`; the
stage fixture creates a short-lived debug container, verifies its runtime
status and logs, and removes its probe Pod before parity capture. Focused crate
tests passed in [nodemigrate checks run 36525276094](https://github.com/centerionware/not-k8s/actions/runs/36525276094)
at SHA `62c4e1c8a2f3e83f56ebd7ffde11abf61d616fbf`; migration-script validation
passed in [run 36525276074](https://github.com/centerionware/not-k8s/actions/runs/36525276074).
The initial test attempt caught and led to correction of an incompatible typed
API helper. No cluster migration has exercised this import path yet.

The goal allows broader, conflict-driven retirement of old control-plane
services or files when an in-place destination needs their ports or paths.
Current code only removes identified kubeadm static-pod sandboxes and targeted
Cilium state; it does not yet retire manifests, old datastore membership, or
other control-plane files. Any broader cleanup must first be implemented with
recovery snapshots and ordered to preserve source quorum until destination
quorum and API/data checks pass. No general old-control-plane teardown has
been verified.

The non-migration K3s+Cilium diagnostic [36509703232](https://github.com/centerionware/not-k8s/actions/runs/36509703232)
completed a same-name Node UID replacement. Workload, storage, all 119 API
resources, and Cilium service datapath checks passed; the API ClusterIP's BPF
backend remained active. It failed only because the parity checker treated
K3s's generated node-password Secret hash rotation as durable user-state
drift. The checker is narrowed to permit that one `/data/hash` mutation in
this diagnostic alone. Bare Node replacement therefore does not reproduce
the post-return Cilium routing failure. Its migration-specific cause remains
unresolved, so no migration rerun is allowed yet.

The corrected checker passed live in [36511406969](https://github.com/centerionware/not-k8s/actions/runs/36511406969)
at SHA `11a401e50ddcc1d0f83d5ecc0970a9b4545bbb89`: source, K3s restart, and
same-name Node replacement each passed fixture, storage, API-inventory, and
normalized state checks, with an active Cilium API ClusterIP datapath. This
still isolates only restart/replacement; the cross-cluster migration-specific
cause needs a concrete fix before nodemigrate is rerun.

Run [36513067748](https://github.com/centerionware/not-k8s/actions/runs/36513067748)
passed temporary Pod TCP probes to the API ClusterIP before restart, after
restart, and after Node replacement, along with all fixture checks. The Cilium
agent Pod UID remained unchanged through these transitions. The next diagnostic
was designed to explicitly recreate the Cilium agent Pod before repeating
these checks. Its first attempt
[36514477496](https://github.com/centerionware/not-k8s/actions/runs/36514477496)
stopped before restart because a jq selector returned a boolean instead of the
Pod object. The selector is fixed, covered by a focused local check, and the
corrected probe result is recorded below. The migration-specific failure
remains unresolved.

The corrected Cilium agent-recreation probe
[36515656678](https://github.com/centerionware/not-k8s/actions/runs/36515656678)
passed at SHA `b3fbd01f5d5bcf438727a7aecd3a515eff9f7f6a`. The replacement agent
Pod reached Ready with a new UID, a temporary Pod connected to API ClusterIP
`10.43.0.1:443`, and workload, storage, resource-inventory, normalized-state,
and Node-replacement assertions passed. This diagnostic does not run
nodemigrate. Recreating the agent Pod alone does not reproduce the
cross-cluster-only failure; cause and fix remain unverified, and migration
must not be retried yet.

Follow-up diagnosis was expanded to mirror the then-current source cutover's
CRI handoff by removing all source Pod sandboxes with Cilium last, restarting
K3s, and repeating the fixture and API ClusterIP checks. The initial live run
[36518048359](https://github.com/centerionware/not-k8s/actions/runs/36518048359)
stopped before service restart when `crictl rmp` returned a canceled RPC for a
stopped sandbox. The probe incorrectly treated it as fatal even though
production tolerates removal failure after confirming no containers remain
running. The harness now mirrors that behavior and a stub verifies it continues
removing later sandboxes. Run
[36519169676](https://github.com/centerionware/not-k8s/actions/runs/36519169676)
was canceled after the migration design changed to avoid bulk sandbox removal;
that probe no longer represents the intended cutover. Migration remains
skipped pending focused verification of the new source shutdown path and a
concrete fix for the post-migration Cilium routing failure.

Latest batch migration run
[36497949349](https://github.com/centerionware/not-k8s/actions/runs/36497949349)
at SHA `1f6b5064936ca2001dea2527483848aebc7e14ad` passed all migration builds
and the upstream Kubernetes three-control-plane/two-worker round trip. The
K3s return leg timed out on same-name Node UID replacement; saved audit events
showed the delete accepted while the old UID was still being updated by K3s.
The rollback workload check then found two Ready nginx Pods after scale-down.
The Docker five-node topology and loss/recovery preflight passed, but forward
import failed during CSINode owner-reference repair because its patch omitted
required `spec.drivers`. These three findings are being fixed together:
bounded confirmation that the old Node UID is gone, valid CSINode repair
patches, and Deployment scale-down convergence. The fixture assertions remain
strict. Focused validation is pending and the migration run has not been
retried. Exact artifacts, jobs, and paths are recorded in
[CI status](NODEMIGRATE_CI_STATUS.md).

Previous batch migration run
[36493677637](https://github.com/centerionware/not-k8s/actions/runs/36493677637) at SHA
`0cef554bd03d53d1725e66a2d47ef2c65edd6dff` passed all nodemigrate and combined
runtime builds but failed all three runtime paths. The new target
ReplicationController controller produced the expected Ready owned Pod in
both single-node lanes; strict parity then flagged only upstream omission
versus target default-zero `spec.minReadySeconds`. Docker import failed because
a source CSINode had `spec.drivers: null`; absent `spec` was already handled,
but null was not. Docker diagnostics also revealed that the replacement API
certificate omitted the source kubeconfig DNS endpoint `cp-1`. All three
findings are fixed together on the branch. The first focused validation at
SHA `5ecbdd7bff9004b4a6da43ebe3155a18f6da0758` caught a missing test-only
`serde_json::Value` qualification; that compile correction is pushed and
awaiting focused revalidation. No new migration run has started. All three
lanes stopped before the return leg. See the [CI status](NODEMIGRATE_CI_STATUS.md)
for exact job IDs and saved logs.

Latest completed migration run
[36468405459](https://github.com/centerionware/not-k8s/actions/runs/36468405459)
at SHA `fe2ca534a3f48a841eaf03f8436fc1343bdb1d8d` passed all nodemigrate and
combined-runtime builds, and the Docker lane built its five-node image. All
runtime lanes failed. Upstream reached returned-stage checks, then failed
strict parity on CRD `double` bounds that changed between adjacent IEEE-754
values; this remains a real numeric difference and no normalization is allowed.
K3s saw the returned Node become Ready and then disappear during the HostPath
CSI setup; the captured audit ends before this transition, so it cannot show
whether a Node DELETE occurred or identify an actor. Docker preflight rejected
four CSINodes whose source omitted the required `spec.drivers`. The logs also expose a repeated
empty CronJob status patch from nodecontroller. Code changes address omitted
empty CSINode drivers and no-op CronJob status writes; both focused regressions
passed in
[run 36473759900](https://github.com/centerionware/not-k8s/actions/runs/36473759900)
at SHA `32255054dcc65baeac828f2ec00acd1b54ba3208`.
The JSON float parsing defect passed focused component verification in
[36479715060](https://github.com/centerionware/not-k8s/actions/runs/36479715060)
at SHA `e253e25cf003ec04f8a59bb9e67988e748732135`; migration parity remains
unverified. K3s Node loss is still unresolved. Run 36490156839 failed during
the nodestore stage before return migration, so it did not exercise the
returned-K3s audit gate or provide Node/Lease actor evidence.
The return path also now requires the replacement Node to have a new UID and
Ready status; focused nodemigrate check
[36481686171](https://github.com/centerionware/not-k8s/actions/runs/36481686171)
passed at SHA `eb650b223a0503e135d4ff50b317d159b39bb2b4`. This guard is not a
runtime fix for the observed K3s controller removal; that cause remains open.
See the [CI status](NODEMIGRATE_CI_STATUS.md) and
[bug tracker](NODEMIGRATE_BUGS.md).

Latest migration run
[36463585411](https://github.com/centerionware/not-k8s/actions/runs/36463585411)
at SHA `b452f071ce9c9a9ac5b3ba6134a93929c1072cf8` built nodemigrate and the
combined branch runtime in all lanes; Docker also built its five-node image.
Upstream passed returned workload behavior but failed strict CRD numeric
round-trip parity. K3s returned the node as Ready, then lost it from API
discovery during the HostPath CSI reinstall. Docker passed topology and
control-plane recovery checks, but its source checkpoint exited 127 because
`python3` was missing from the node image; the image now installs it pending
rerun. None completed the required full bidirectional gate. See the
[CI status](NODEMIGRATE_CI_STATUS.md) for exact jobs and artifacts and the
[bug tracker](NODEMIGRATE_BUGS.md) for unresolved causes.

Latest migration evidence is run
[36434878771](https://github.com/centerionware/not-k8s/actions/runs/36434878771)
at SHA `4f844b4311284e93fa6c8883c7858ae87122ee8a`. All lanes compiled
nodemigrate and branch `notk8s --features cri` successfully, so every modified
component packaged in the combined runtime was included in the build. Docker
also compiled its five-node image. K3s passed source and nodestore checkpoints,
reported a successful return, then failed while HostPath CSI failed to become
Ready. Upstream reached returned-stage parity but its returned StatefulSet
could not mount its PVC: HostPath CSI was not registered and then reported the
retained volume ID missing from its catalog. Docker's five-node source fixture
failed its Cilium Helm-values assertion after the source workloads and PV data
probes passed. These three failures are unresolved; no bidirectional migration
gate has passed. Run artifacts are saved once under
`/tmp/nodemigrate-artifacts-36434878771/`. No regular build or full e2e ran.

Follow-up run
[36459506591](https://github.com/centerionware/not-k8s/actions/runs/36459506591)
at SHA `da02155289d2bfdf02cbe32d0885b941f5f8542a` compiled nodemigrate and
the combined branch runtime for K3s and upstream, and also compiled the Docker
five-node runtime/image. Cilium KPR was enabled and the full five-node round
trip was requested. K3s again failed to bring HostPath CSI Ready after return.
Upstream passed the returned workload checkpoint but produced `ClaimMisbound`
events for fixture PVCs and then failed mounting the existing StatefulSet CSI
volume because its ID was missing from the driver's catalog. Docker failed
before migration because its Cilium check read Helm overrides and missed the
effective default for `cni.confPath`; the current branch now checks merged
values. A code fix orders PVC import before PV import and maps the PV's claim
reference before its initial write to avoid temporarily publishing a PV without
its claim UID. Ordering and remapping unit regressions passed the complete
nodemigrate crate test in [run 36486806547](https://github.com/centerionware/not-k8s/actions/runs/36486806547)
at SHA `899143621b5aaf944a3ce0ab50ef97f4f547bd92`. A migration rerun is still
required to verify the CSI catalog/data handoff. No required round-trip gate
has passed.

The focused nodemigrate check
[36425866401](https://github.com/centerionware/not-k8s/actions/runs/36425866401)
passed at SHA `8b11b11f3f26546f0a08bdcaba99382bb2fdcb6b`. Migration run
[36425151654](https://github.com/centerionware/not-k8s/actions/runs/36425151654)
compiled nodemigrate and the branch combined runtime in all lanes, but all
three runtime lanes failed. Docker's static-data probe could not schedule on
the tainted cp-1 PV node; its fixture now has a control-plane toleration. K3s
reported completed return migration, then its Node was absent during later
verification; the logs do not identify what removed it. Upstream returned
Traefik CRD schema bounds with a real numeric difference. The current importer
patches metadata alone when an existing CRD schema matches, so it does not
round the schema through a full-object update. These changes await migration
CI. The K3s Node lifecycle remains unresolved. No general build or full e2e
ran. The return-target state watcher now continues through returned-stage
workload and parity checks to capture a Node loss after nodemigrate reports
success. Focused test run
[36429944327](https://github.com/centerionware/not-k8s/actions/runs/36429944327)
failed compiling the metadata-patch regression because its `Value::Null`
assertion was not qualified in the test module; the reference is fixed and the
focused rerun passed. Migration run
[36430036471](https://github.com/centerionware/not-k8s/actions/runs/36430036471)
is testing this commit across the enabled topologies.

The corrected focused nodemigrate check
[36430618866](https://github.com/centerionware/not-k8s/actions/runs/36430618866)
passed at SHA `37da8cde9d1bc2489bfe588d379499b55b929e39`, including crate tests.
Migration run `36430036471` has built nodemigrate and the combined branch
runtime (`notk8s --features cri`) in both single-node lanes, built the five-node
image, then completed with all three runtime lanes failing. Docker passed both
static and CSI PV data probes before its source-stage verifier exited after
Traefik rollout; the exact silent Helm assertion is not yet identified, so
those checks now report their own failures. K3s completed return migration and
reported its Node Ready, then the state watcher captured an empty Node list at
13:59:32 UTC while returned-stage checks failed on the missing local-PV Node.
Upstream reached `PASS stage=returned`, but strict parity still found both
Traefik CRD maximums changed from `9223372036854775000` to
`9223372036854776000`. The CRD metadata-only patch did not run for those two
objects; a focused comparison diagnostic now records the existing and desired
schema values. No general build or full e2e ran.

## Latest validation

The nodemigrate check
[36424188417](https://github.com/centerionware/not-k8s/actions/runs/36424188417)
and branch-runtime migration dispatch
[36424227253](https://github.com/centerionware/not-k8s/actions/runs/36424227253)
failed compiling the new CRD predicate at SHA
`b140e73d7e57fdafdcb9105767b9e6056954abb6`: `DynamicObject.types` is an
`Option<TypeMeta>`. Its first correction then failed in nodemigrate run
[36424544812](https://github.com/centerionware/not-k8s/actions/runs/36424544812)
and migration dispatch
[36424565854](https://github.com/centerionware/not-k8s/actions/runs/36424565854),
because `TypeMeta.kind` is a `String`. The predicate now guards optional
TypeMeta and directly compares that string. The branch migration build at SHA
`46428806c3490958bd76a033c9e443391f2694aa` compiled nodemigrate in both
single-node lanes; run
[36425151654](https://github.com/centerionware/not-k8s/actions/runs/36425151654)
is still running through the combined runtime builds and migration checks. The
matching test run
[36425123437](https://github.com/centerionware/not-k8s/actions/runs/36425123437)
failed compiling only the new unit-test JSON literals, which need `u64`
suffixes; that correction is in the current worktree and must be pushed and
tested. No general build or full e2e ran.

## Latest integration result

Branch-runtime run
[36420315264](https://github.com/centerionware/not-k8s/actions/runs/36420315264)
at SHA `de890b701d61130b3e6ce9922ffc5597e4342121` built nodemigrate and the
combined branch runtime (`notk8s --features cri`) in all three lanes, thereby
compiling the packaged component changes; Docker also built the five-node
image. Docker passed its Cilium KPR and control-plane recovery checks plus CSI
registration on the plugin Node, then fixture setup hit an unsupported
`kubectl version --client -o jsonpath` option. That query now extracts
`gitVersion` from YAML. Upstream reached the returned checkpoint with source
and nodestore Traefik CRD bounds still exactly `9223372036854775000`; the
original API rounded the value during return import. `apply_object` now avoids
rewriting an existing CRD when its spec, labels, and annotations match, with a
focused unit regression for unchanged and changed CRDs. K3s reported successful
return migration, then the returned Node was absent from later API snapshots;
kubelet lost node authorization for imported Pods and CSI teardown/readiness
failed. The capture does not identify what removed the Node. Runtime checks for
the current fixes are pending, and full migration parity remains unverified.
No general build or full e2e ran.

## Latest integration result

Branch-runtime run
[36418026366](https://github.com/centerionware/not-k8s/actions/runs/36418026366)
at SHA `7e8337a08a73e6027ceaab5605eacd9acebaa0de` compiled nodemigrate and the
combined branch runtime (`notk8s --features cri`) in all three lanes; Docker
also built its five-node image. The five-node cluster passed Cilium KPR,
control-plane loss/recovery, and hostpath CSI registration on the Node running
the singleton CSI plugin. Fixture setup then stopped because its standalone
entrypoint left `NODEMIGRATE_KUBECTL_IMAGE` unset. K3s and upstream both
completed source setup and forward import into nodestore, then failed while
collecting their nodestore checkpoint: the added CRD precision probe used the
unsupported `crd` kubectl short name. The probe now uses the fully qualified
CRD resource, and the fixture derives its kubectl image from the running
client. No nodestore checkpoint, return migration, or precision comparison
was reached in this run. Artifacts are under
`/tmp/nodemigrate-artifacts-36418026366/`. Corrected runtime verification is
pending; no general build or full e2e ran.

## Latest integration result

Branch-runtime run
[36414625541](https://github.com/centerionware/not-k8s/actions/runs/36414625541)
at SHA `b5bd5bded1e3016dbffebc6e68f3956f495c7883` compiled nodemigrate and the
branch combined runtime (`notk8s --features cri`) in all three lanes; the
five-node lane also built its node image. Docker verified the five-node Cilium
KPR cluster recovered all five Nodes after cp-1 loss, then stopped before
migration because the fixture incorrectly required the singleton hostpath
CSI StatefulSet to register on all Nodes. The driver Pod was Ready on
`worker-2`; the verifier now checks every Node that actually runs the plugin.
K3s passed source and nodestore checkpoints and completed migration back, then
its hostpath deployment did not become Ready within five minutes. Captured
logs show the K3s Node later absent while kubelet/Lease reconciliation failed,
and hostpath reported missing driver registration/volume catalog state; the
initiating Node/Pod transition remains undetermined. Upstream passed source,
nodestore, and returned workload/storage checks, but strict parity still found
Traefik CRD `priority.maximum` changed from `9223372036854775000` to
`9223372036854776000`. This is a real numeric difference and remains a blocker.
Artifacts are saved under `/tmp/nodemigrate-artifacts-36414625541/`; the CSI
verifier fix and runtime follow-up are pending. No general build or full e2e
ran.

## Latest integration result

Branch-runtime run
[36411280751](https://github.com/centerionware/not-k8s/actions/runs/36411280751)
at SHA `14bbe10d8b15dc87fd6ac91b8bdef5d13c6db648` built nodemigrate and the
branch combined runtime (`notk8s --features cri`) in every migration lane,
therefore building all packaged components modified on this branch. The
five-node job also built its node image. Docker preflight fixed the CoreDNS
forwarding loop, but Cilium then failed to resolve `cp-1` while bringing up
the API connection. The fixture now passes Cilium cp-1's reachable IP directly.
K3s returned the replacement Node as Ready and completed the migration, but
the post-return hostpath CSI deployment did not become ready amid repeated
Cilium and Node readiness churn; the logs do not identify the initiating
actor. Upstream reached returned-stage checks, then strict parity again found
Traefik CRD schema `priority.maximum` values changed from
`9223372036854775000` to `9223372036854776000`. This remains a real mismatch;
the responsible hop has not been isolated. Focused nodemigrate crate tests
passed at this SHA in
[36411280754](https://github.com/centerionware/not-k8s/actions/runs/36411280754).
Artifacts are saved under `/tmp/nodemigrate-artifacts-36411280751/`. The
focused follow-up is pending; no general build or full e2e ran.

## Latest integration result

Branch-runtime run
[36407648434](https://github.com/centerionware/not-k8s/actions/runs/36407648434)
at SHA `7309de99f5d653961d8e4ce2a331b655d07924ee` built nodemigrate, the branch
combined runtime (`notk8s --features cri`), and the five-node image, then failed
in all three runtime lanes. In the five-node kubeadm/Cilium fixture, captured
CoreDNS logs confirmed its `loop` plugin forwarding to `127.0.0.1`; the source
NetworkPolicy allow probe then timed out. The fixture is being corrected to
give kubelets an explicit, non-loopback upstream resolver file. In K3s, the
returned node reached Ready, but its CSINode disappeared between the read and
owner-reference patch, which failed with 404. A CSI registrar replacement
race is the leading explanation, but the logs do not identify the actor. The
utility now retries that specific 404 under its existing 60-second deadline.
Upstream Kubernetes reached return-stage workload and storage checks, then
strict parity caught the Traefik CRD integer `maximum` changing from
`9223372036854775000` to `9223372036854776000`. That real difference remains
unresolved; parity has not been relaxed. Artifacts are saved under
`/tmp/nodemigrate-artifacts-36407648434/`. The new fixes await a fresh focused
migration run.

Branch-runtime run
[36402381606](https://github.com/centerionware/not-k8s/actions/runs/36402381606)
completed with failure at tested SHA `8ce9b2691f0c639f10017f55edfa23fd094ad500`.
All lanes built nodemigrate and the branch combined runtime (`notk8s --features
cri`); the five-node lane also built its node image. Kubernetes reached
`PASS stage=returned` before strict CRD `float64` value parity failed. K3s
returned the replacement Node as Ready but later verification found no Node;
the initiating deletion is unconfirmed. The five-node source fixture failed
its NetworkPolicy allow probe with CoreDNS crash-looping, and diagnostics did
not capture the container failure output. Follow-up diagnostics now capture
CoreDNS logs plus Node/Lease and service journal state.
The corrected focused nodeapiserver check passed at SHA `4c718dc0` in
[36403498362](https://github.com/centerionware/not-k8s/actions/runs/36403498362).
The preceding completed branch-runtime failure at SHA
`fa8c6ec97c3a3afecd1162304bcecc0694f19a1d` was run at
[36397547121](https://github.com/centerionware/not-k8s/actions/runs/36397547121).
That run's focused nodemigrate crate tests passed at
[36397531245](https://github.com/centerionware/not-k8s/actions/runs/36397531245).
Upstream completed its return workload and storage checks, then strict
round-trip parity again found two Traefik CRD `priority.maximum` values change
from `9223372036854775000` to `9223372036854776000`. A nodeapiserver CRD
double round-trip check exposed an incorrect assertion about the original
integer-shaped JSON spelling; it now checks the protobuf double's numeric
value and passed in the later quick-check. K3s completed API import
and returned the replacement Node to Ready, but failed because the CSI
registrar had not yet recreated its `CSINode`. `nodemigrate` now waits for up
to 60 seconds for that specific object before repairing its owner reference.
The five-node kubeadm fixture built successfully but could not schedule its
seed Pod: the static PV is on `cp-1` and the CSI PV is on `worker-2`. The
fixture now seeds and verifies the two volumes in separate Pods. Its
diagnostics also showed CoreDNS CrashLoopBackOff, still under investigation.
These fixes await another migration run. Full logs:
`/tmp/nodemigrate-36397547121.log`.

Branch-runtime run
[36385002094](https://github.com/centerionware/not-k8s/actions/runs/36385002094)
at SHA `9c7897f0d917e51b9e3d8e81ee6a843e3332c8f2` failed before completing
either full round trip. All three jobs built `nodemigrate` and branch
`notk8s --features cri`, compiling the combined runtime's packaged component
crates; targeted nodemigrate quick-check passed at
[36384989298](https://github.com/centerionware/not-k8s/actions/runs/36384989298).
The Docker source fixture exposed a static hostPath PV without node affinity;
its directory existed only on cp-1, so the PV is now pinned to that node. K3s
passed retained API readiness but timed out on repeated per-group discovery
requests while Cilium was unhealthy; aggregated discovery is now attempted
first, with a per-group fallback for compatibility. Upstream completed both
migration directions and returned-stage checks for 119 listable APIs,
Ingress/Gateway, PVC data, StatefulSet PVs, CronJob/Job, RBAC, and certificates.
Strict parity then found changed Traefik CRDs, destination-generated
ReplicaSets/ControllerRevisions, and `nodebootstrap:` RBAC absent from the
source. These differences are unresolved, so the full round-trip gate remains
red. Logs are in `/tmp/nodemigrate-{docker,k3s,kubernetes}-36385002094.log`;
artifacts are in `/tmp/nodemigrate-artifacts-36385002094/`. No general build or
full e2e workflow was dispatched.

Previous branch-runtime run
[36381052266](https://github.com/centerionware/not-k8s/actions/runs/36381052266)
at SHA `74352dbd90ffa3df71ff5adb208adf93c9dad1f7` failed all three runtime
jobs after both binaries built successfully. Targeted nodemigrate unit tests
passed at the same SHA in
[36381051939](https://github.com/centerionware/not-k8s/actions/runs/36381051939).
K3s forward migration and workload setup reached the nodestore checkpoint;
rollback completed, but the non-root `emptyDir` probe selected a Ready Pod
whose Cilium-replaced CRI sandbox had exited. The probe now requires the
API-reported container state to be running and retries the observed transient
`CONTAINER_EXITED` exec response while polling fresh Pod state. Upstream
completed return import, recreated the source-running standalone Pod, and
re-registered the returned Node. All returned-stage resource checks passed
until both Traefik routes returned 504. Diagnostics show the migration's
reverse export selected 11 EndpointSlices generated by this project's
`nodecontroller`; nodemigrate now skips that manager label so the destination
controller can rebuild live endpoints. The five-node preflight passed
container isolation, kubeadm setup, cp-1 loss/recovery, all-node readiness, and
Cilium rollout after restoring shared mount propagation. It then failed while
installing hostpath CSI because `git` was missing in the node image; the image
now installs `git` and `cri-tools` for setup and failure diagnostics. These
follow-up fixes need a new branch-runtime run. No general build or full e2e
workflow was dispatched.

Branch-runtime run
[36377385003](https://github.com/centerionware/not-k8s/actions/runs/36377385003)
at SHA `d2a7e06abeed8ce349fd8c53795809486004c876` is terminal failure. All
three lanes built `nodemigrate` and branch `notk8s --features cri`; that
combined binary build compiles the packaged runtime component crates, including
their branch changes. K3s completed forward migration and 119-resource parity,
then its return rolled back after the fixture selected a Running Pod whose
container had exited. The selector now requires a non-terminating Pod with a
Ready condition and all containers Ready. Upstream completed forward and
return import and replacement Node readiness; its transient cert-manager
webhook HTTP 500 recovered through the bounded retry. The return checkpoint
then found a standalone Pod that had been Running in the source but had
terminated with exit 137. The exporter now records source-running Pod state,
and the importer recreates a terminal same-name Pod from the source spec.
The five-node Docker preflight built both binaries and recovered the Kubernetes
API and all Nodes, but Cilium remained unready after cp-1 restart because
`/sys/fs/bpf` was not shared with the CNI mount namespace. The preflight now
restores shared propagation for `/sys`, bpffs, and `/run` before kubelet starts.
These latest fixes are pending migration CI. No general build or full e2e
workflow was dispatched.

The latest run
[36373995554](https://github.com/centerionware/not-k8s/actions/runs/36373995554)
used branch runtime at `539ea445a7d7bc1c73e94ffe5c759eb5e9232751`. All three
jobs compiled `nodemigrate` and `notk8s --features cri`, compiling every
packaged runtime component crate. The upstream lane completed forward
migration, retained API startup, and CRD import; reference repair then failed
on a CertificateRequest because the cert-manager webhook Service refused the
connection. The K3s lane completed forward migration and API/resource parity;
returning from nodestore triggered Cilium init-container restarts, loss of Pod
and Service networking, and an `emptyDir` exec against a container in
`CONTAINER_EXITED` state. The fixture did not check Cilium/CoreDNS readiness
before application probes. The five-node preflight recovered cp-1 after one
failure-isolation cycle but repeated the same stop/start and failed its second
recovery gate without per-check diagnostics. The new code retries transient
errors while repairing imported references; the fixture now waits for Cilium
and CoreDNS, and the Docker preflight performs one diagnosed recovery cycle.
Verification of these changes is pending. No general build or general e2e
workflow ran.

The follow-up run
[36371692313](https://github.com/centerionware/not-k8s/actions/runs/36371692313)
finished at code SHA `dfa3325594be1b150f07576f0ecc2060f7554deb`. Focused
nodemigrate crate tests passed at that SHA in
[36371692594](https://github.com/centerionware/not-k8s/actions/runs/36371692594).
All migration lanes compiled `nodemigrate` and the branch `notk8s` combined
binary with CRI, including the modified runtime components. The upstream lane
completed return API import, restarted the retained kubelet service, and
observed the replacement Node become Ready; it then failed because the fixture
eviction of an nginx Pod succeeded while the PDB check had observed
`disruptionsAllowed=0`. The log shows old and replacement nginx Pods during
rollout, so the harness currently may race an overlapping or terminating Pod;
the fixture now waits for exactly one non-terminating Ready replica and records
PDB/Pod/response details if eviction is unexpectedly accepted. K3s still fails
all 20 full discovery probes after return readiness. The five-node Kubernetes
and Cilium preflight recovered after control-plane loss, but full migration
fixture setup failed when `cp-1`'s API endpoint was still refusing connections;
the preflight now waits for the restarted API, all Nodes, and Cilium before
starting migration. These fixes await another branch-runtime run. No general
build or e2e gate was run.

Branch-runtime migration run
[36369153385](https://github.com/centerionware/not-k8s/actions/runs/36369153385)
at SHA `1d37976d` completed the five-node Docker preflight and both builds.
Both Cilium KPR round trips still failed. K3s passed retained API readiness,
then full API discovery timed out on all 20 probes; the same log shows Cilium
Service traffic failing with `no route to host`. Upstream returned API
readiness, accepted all 55 CRDs, and completed the protected API import, but
the returned Node disappeared after initially reporting Ready and workloads
became unschedulable. The integration sets `NODEMIGRATE_REPLACE_NODE=true`;
the return path deleted the registered Node after activating the target and
did not restart kubelet. The current branch now restarts the retained service
after deleting a replacement Node so it can register again. This change has
not yet been migration-tested. Identity records were skipped as intended, but
that did not make either round trip pass. See the [CI record](NODEMIGRATE_CI_STATUS.md)
and [bug tracker](NODEMIGRATE_BUGS.md) for exact component findings.

Branch-runtime run
[36335580680](https://github.com/centerionware/not-k8s/actions/runs/36335580680)
at SHA `f186930d7333a0b1c96967f2418b9be18b1155d6` passed the Docker
five-node preflight and both utility/runtime builds. The upstream lane passed
source and nodestore PV/PVC/data checks and completed forward migration. Its
return import then timed out on the cert-manager webhook Service ClusterIP;
nodemigrate restored nodestore and retained the export. The K3s lane was
cancelled at the workflow timeout, so the overall run failed. The upstream
target log confirms a running kube-proxy Pod and active nodeproxy together
with Cilium KPR disabled; fix `7bad3b91` selects one Service router from KPR
and kube-proxy DaemonSet state. A target snapshot also showed Cilium agent
and Envoy unready and CoreDNS/cert-manager containers terminated with exit
137; the cause remains unconfirmed. Rollback fixture verification separately
observed a Pod disappear before `kubectl exec`, so the non-root `emptyDir`
marker failure is not yet attributed to migration. The upstream log contains
exactly 226 jq parse errors from the watcher in the pre-fix runtime at
`f186930d`; the `deaed970` fix captures
`kubectl` stderr separately from JSON and emits structured request/parse
diagnostics. Its focused diagnostic check passes, and PR validation at the
current docs SHA passed. A post-fix migration run has not yet confirmed the
noise is gone from a full migration log. The follow-up diagnostics at
`bb32d6c8` also capture kube-proxy readiness. Neither
lane has passed a full round trip, and no PV/PVC/backing-volume deletion was
reported. Full lane evidence is recorded in the [CI status](NODEMIGRATE_CI_STATUS.md).

The migration fixture installs Cilium 1.20.2 with
`kubeProxyReplacement=false`. Its upstream stages therefore use kube-proxy;
the `kube-proxy` watcher output is diagnostic, and an empty result is valid
for a Cilium KPR cluster. At `7c1c0b1b`, joined-worker bootstrap reads
`kube-system/cilium-config` and passes `--proxy=none` when Cilium KPR is
enabled. Run 36335580680 confirmed that the imported upstream kube-proxy Pod
and `nodeproxy` were both active when KPR was disabled. Fix `7bad3b91` detects
the source kube-proxy DaemonSet, preserves it as the Service router, and
disables nodeproxy; joined workers inspect the destination API. Cilium KPR
and kube-proxy enabled together now fail before cutover. The migration
checkpoint requires exactly one router. K3s still uses nodeproxy when its
embedded kube-proxy stops. Focused crate checks are running; runtime evidence
for this ownership fix is pending. Plan output and logs report the selection.
The focused nodemigrate quick-check
[36342042535](https://github.com/centerionware/not-k8s/actions/runs/36342042535)
passed. KPR-enabled single-node migration run
[36343008296](https://github.com/centerionware/not-k8s/actions/runs/36343008296)
is in progress: its upstream lane failed after 43m18s and its K3s lane remains
active. The run predates the initial-bootstrap proxy fix; no post-fix KPR
round trip is verified. Per-node Cilium overrides remain open.
Cilium's Envoy L7 proxy remains a separate component. PR crate tests and
workflow validation passed for branch head `71dd6c87` in
[36342224121](https://github.com/centerionware/not-k8s/actions/runs/36342224121)
and [36342224306](https://github.com/centerionware/not-k8s/actions/runs/36342224306).
See the expanded requirement in [NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md).

For staged cluster replacement, a later node must not write host-path or CNI
recovery data into the first node's protected export. Forward control-plane,
forward worker, and staged reverse control-plane paths now deep-copy a supplied
`source-export=` into a new mode-restricted node-local export before use. The
first export remains intact for remaining nodes; each control-plane's PV/CNI
backup is attached only to its private copy, while each worker keeps its
separate local-volume snapshot. A focused unit regression and migration
workflow static validation passed at code SHA `59b38a96` in runs
[36348554916](https://github.com/centerionware/not-k8s/actions/runs/36348554916)
and [36348554980](https://github.com/centerionware/not-k8s/actions/runs/36348554980).
The strengthened regression also checks copied Node identity/scheduling
metadata and passed at SHA `f5bcadbb` in
[36349107771](https://github.com/centerionware/not-k8s/actions/runs/36349107771);
workflow validation passed in
[36349107726](https://github.com/centerionware/not-k8s/actions/runs/36349107726).
Multi-node runtime verification remains pending.

The dedicated integration workflow now accepts `cilium_kpr=false|true`.
For `true`, it disables the K3s kube-proxy or skips kubeadm's kube-proxy
addon, configures Cilium KPR, and fails the source fixture if kube-proxy's
DaemonSet remains; the nodestore checkpoint also fails if nodeproxy is active.
Code review found that `sudo --preserve-env` dropped the KPR option before the
root-run fixture; the workflow now forwards it. Validation for this fix is
pending.
The five-node kubeadm/Cilium preflight now receives the same KPR input, skips
the kube-proxy addon when enabled, configures Cilium accordingly, and asserts
the DaemonSet is absent. Enabled-KPR preflight
[36345278305](https://github.com/centerionware/not-k8s/actions/runs/36345278305)
passed at SHA `74068255`: five isolated nodes became Ready, Cilium reported
KPR enabled with kube-proxy absent, and the cluster recovered after control
plane loss. This validates the five-node simulation setup only; it did not run
nodemigrate. Earlier preflight runs used KPR disabled.
Workflow validation passed at `027ee68d` in
[36342627911](https://github.com/centerionware/not-k8s/actions/runs/36342627911);
nodemigrate crate tests passed in
[36342627913](https://github.com/centerionware/not-k8s/actions/runs/36342627913).
The KPR-enabled live migration was dispatched at `7f1c53f1` in run
[36343008296](https://github.com/centerionware/not-k8s/actions/runs/36343008296).
Its upstream lane failed after 43m18s, the K3s lane remains active, and the
failure cause is pending artifact review after the entire run is terminal.
While it runs, code review found that the first control-plane bootstrap also
needs to disable nodeproxy when the imported Cilium config enables KPR; the
existing change covered joined workers only. The initial-bootstrap path now
reads this setting from the live source API or protected export, passes
`--proxy=none`, and reports the selected mode. Focused nodemigrate quick-check
[36343511798](https://github.com/centerionware/not-k8s/actions/runs/36343511798)
passed at code SHA `da0fab88`. The active KPR-enabled run predates this fix and
is evidence only for the prior implementation; a post-fix KPR-enabled
migration run remains pending.

The staged reverse control-plane export change is at `3f807ba8`. Its first
crate run, [36340951765](https://github.com/centerionware/not-k8s/actions/runs/36340951765),
found a fixture assertion still expecting one Node after the fixture gained a
control-plane and worker entry. The assertion now verifies both entries and
that only the control-plane node is selected. Nodemigrate crate tests passed
at `ec6c02c1` in [36341187596](https://github.com/centerionware/not-k8s/actions/runs/36341187596);
workflow validation passed at the same SHA. No local Cargo build/test was run.

The nodemigrate quick-check passed at utility SHA `17281efb42343ae2f934f681f57f031a59bc6016`
in [36315451874](https://github.com/centerionware/not-k8s/actions/runs/36315451874).
The fixture failure at
[36315595317](https://github.com/centerionware/not-k8s/actions/runs/36315595317)
was fixed at SHA `b69ef4e7` by creating the legacy token Secret with its
required ServiceAccount annotations.

At code SHA `ad861cdc7f57876c1c865a7d0b42a2b52bd3b8d4`, branch-runtime
migration [36320456287](https://github.com/centerionware/not-k8s/actions/runs/36320456287)
passed the five-node Docker kubeadm/Cilium preflight and both single-node
source and nodestore stages. PV/PVC bindings and data, StatefulSet scale/data,
token/RBAC, ingress, certificate, 119-resource API inventory, strict API
object parity, and the source CSI `/dev` volume before and after fixture setup
passed in both lanes. Application snapshot comparison then differed only
because the target API returned `StatefulSet.spec.minReadySeconds: 0` where
source omitted the API default. Neither lane reached return migration. No PV,
PVC, or backing-volume deletion was reported. Artifacts are at
`/tmp/nodemigrate-36320456287-artifacts/`. The five-node job was a preflight
only and did not migrate the cluster. The local `check_nodemigrate_snapshot_filter.sh`
now covers this API default and confirms nonzero settings remain strict; the
dedicated migration rerun is pending.

The previous run at SHA `a95833a4a32122ba2b91b4d3c95f40db5363e491`,
[36318123614](https://github.com/centerionware/not-k8s/actions/runs/36318123614),
appeared to show the CSI `/dev` volume missing. Run 36320456287 established it
is present immediately after migration; the earlier mismatch followed the
test harness's CSI reinstall. Focused nodeapiserver schema-pruning and
protobuf-storage round-trip checks passed in [36319639827](https://github.com/centerionware/not-k8s/actions/runs/36319639827)
and [36319953565](https://github.com/centerionware/not-k8s/actions/runs/36319953565).

The preceding run at SHA `f9356dad16a549fdaf07c2a79e135a8cbfc836e9`,
[36313589064](https://github.com/centerionware/not-k8s/actions/runs/36313589064),
completed with both migration jobs failing after Docker preflight and the
utility/runtime builds passed. Both lanes passed source and nodestore stages,
including PV/PVC binding, data, and all 119 source-discovered API resources.
Strict object comparison failed on the test driver's nodelet-root CSI
paths/source-stage mount and regenerated `CiliumNode.spec.health.ipv4`;
neither lane reached return migration. No PV deletion was reported. Logs are
at `/tmp/nodemigrate-36313589064-k3s.log` and
`/tmp/nodemigrate-36313589064-kubernetes.log`.

The branch replaces same-name destination objects with an optimistic full
update so omitted source fields do not survive as hybrid objects, and repairs
Node-owned references after target Node registration. The checkpoint
normalizer treats only documented controller/default/API version fields as
lifecycle differences; source object specs and user rollout history remain
checked. SHA `17281efb` also preserves and reissues legacy ServiceAccount
token Secrets and exercises their RBAC at each checkpoint. The corrected
fixture let the current run verify token/RBAC behavior at source and nodestore;
no return-stage token result is claimed because CSI parity stopped both lanes.

Run [36299240279](https://github.com/centerionware/not-k8s/actions/runs/36299240279)
at SHA `20648debff0f7f338c3e37303bbaf367ffa449a4` passed the five-node
Docker/kubeadm/Cilium preflight and both utility/runtime builds. In K3s, the
source and nodestore workload checks passed, including Deployment scale and
StatefulSet ordinal-0 data. The target then failed source-discovered API
inventory because `nodes.metrics.k8s.io` and `pods.metrics.k8s.io` were
missing. The metrics APIService reported Available, while metrics-server
logged `x509: certificate signed by unknown authority` scraping nodelet; the
precise link between discovery and scrape failure still needs confirmation.
CSI also registered dynamically but the hostPath driver reported a migrated
volume handle missing from its volumes list. Neither lane reached return
migration. Upstream passed its stage behavior/resource checks, then strict
source-object parity found changed fields in CoreDNS, ReplicaSets,
StatefulSets, Leases, RBAC, PriorityClasses, CSINode, CSIDriver, and CiliumNode.
These remain failures; no normalization was added. Full logs:
`/tmp/nodemigrate-36299240279/nodemigrate-k3s-36299240279/nodemigrate-k3s.log`
and
`/tmp/nodemigrate-36299240279/nodemigrate-kubernetes-36299240279/nodemigrate-kubernetes.log`.

Run [36297966919](https://github.com/centerionware/not-k8s/actions/runs/36297966919)
at SHA `d2eebede56084347cbc8eeba03fb12336bed7e6e` passed the Docker
five-node kubeadm/Cilium preflight and built both `nodemigrate` and the branch
runtime. The K3s forward migration completed and target API readiness passed;
the target then failed the fixture's strict Deployment scale check. The same
lane logged that the unchanged hostPath CSI driver had not registered with
nodelet, and CSI `NodeStageVolume` failed. The metrics APIService was
`Available=True`, but metrics-server could not scrape the nodelet kubelet due
to its serving certificate chaining to an unknown CA. No K3s return migration
ran.

The upstream lane passed its source and nodestore behavior checks, including
all 119 listable API resources exposed at the target. It then failed the
strict source-object comparator on same-identity changes in CoreDNS, controller
replica counts, Leases, system RBAC/PriorityClasses, CSI, and Cilium objects.
Those fields remain failures until their preservation or regeneration behavior
is fixed and verified. Neither lane reached the return leg or round-trip gate.
The complete lane logs are
`/tmp/nodemigrate-36297966919-k3s.log` and
`/tmp/nodemigrate-36297966919-kubernetes.log`.

Run [36295890192](https://github.com/centerionware/not-k8s/actions/runs/36295890192)
at SHA `57fd9ca6305ecb79c45b31513e82de4a3ce46c59` passed Docker five-node
preflight and both utility/runtime builds. The K3s lane migrated through the
nodestore workload checkpoint, then failed source API discovery parity because
`nodes.metrics.k8s.io` and `pods.metrics.k8s.io` were unavailable: metrics-server
could not authenticate to the destination API using the source
`extension-apiserver-authentication` ConfigMap's old front-proxy CA. The
current worktree regenerates this destination-owned ConfigMap from destination
PKI and classifies source trust bundles as destination-generated state. The
upstream lane reached strict object parity and found same-identity differences
in CoreDNS, ReplicaSets/StatefulSets, Leases, system RBAC/PriorityClasses, CSI,
and Cilium node state. These remain failures until their behavior/lifecycle is
verified; no return migration or round-trip comparison passed. The current
trust fix has not yet run in CI.

Run [36258945785](https://github.com/centerionware/not-k8s/actions/runs/36258945785)
used branch SHA `bb6d20a99d31c6e015dee5bf65d1188ff3bf7e43`. Both migration
utility and combined runtime builds passed after the Docker five-node
preflight. K3s+Cilium failed while importing one CertificateRequest because
the cert-manager admission webhook was not yet reachable; the importer had
only five retries at three-second intervals. Its source rollback and
protected export retention passed. Upstream Kubernetes+Cilium completed
forward migration and target checks, then failed only the fixture semantic
snapshot on destination-generated root CA annotation metadata and optional
PVC-template TypeMeta. The separate active-API-CA content assertion passed.
Neither lane ran the return migration, so bidirectional parity is unverified.

The current branch extends retry handling to a bounded five minutes for
transient API/webhook and transport failures, while permanent rejections fail
without waiting. The application snapshot now omits only the generated
namespace root CA ConfigMap and ignores optional `apiVersion`/`kind` fields in
StatefulSet PVC templates. In run
[36260417450](https://github.com/centerionware/not-k8s/actions/runs/36260417450),
upstream Kubernetes passed source and nodestore target checks, then failed
return import after five minutes: Certificate, CertificateRequest, and
ClusterIssuer writes timed out reaching `10.97.233.241:443`; one
PersistentVolume write returned 404. The request path was not captured, and
the cause of both failures is unconfirmed. No returned-source checkpoint or
parity passed. The K3s lane was later cancelled before completion. New failure capture watches
the return target and probes the webhook ClusterIP and ready EndpointSlice
addresses; restore errors now include their API request path. Focused
nodemigrate tests [36263787654](https://github.com/centerionware/not-k8s/actions/runs/36263787654)
and migration workflow validation
[36263787652](https://github.com/centerionware/not-k8s/actions/runs/36263787652)
passed at `434c9acb`. The `nodemigrate` quick-check
[36260417435](https://github.com/centerionware/not-k8s/actions/runs/36260417435)
passed at SHA `8c79470de60f288fc113db7b7b8c45da048b6ad7`. Branch-runtime migration
[36260417450](https://github.com/centerionware/not-k8s/actions/runs/36260417450)
completed its upstream lane with the failure above; its K3s lane was later
cancelled while still running. Round-trip parity remains unverified.

The migration fixture now snapshots every deployed Helm release at each
source, nodestore, and returned-source checkpoint. It records release name,
namespace, chart and app version, revision, and SHA-256 digests for computed
values and rendered manifest; source-to-target and full round-trip assertions
compare those records. This closes a test-evidence gap where the fixture
inspected Helm state and ran server-side dry-run upgrades but did not compare
release records across stages. Shell syntax, snapshot-filter checks, diff
validation, and a stubbed Helm snapshot check passed locally. Runtime
verification is pending; run `36260417450` predates this assertion and remains
in progress for K3s as of 2026-09-26 19:21 UTC. On the latest PR SHA
`32e8ccb2`, migration workflow validation [36265673005](https://github.com/centerionware/not-k8s/actions/runs/36265673005)
and focused `nodemigrate` crate tests [36265672968](https://github.com/centerionware/not-k8s/actions/runs/36265672968)
passed. The dedicated migration runtime jobs were skipped on the pull-request
validation event; no new runtime migration was started while the earlier K3s
lane remained active.

The fixture now also carries a `policy/v1` PodDisruptionBudget for its nginx
Deployment, checks the healthy/disruptions-allowed status at every checkpoint,
and attempts the Pod eviction subresource. The expected result is HTTP 429 while
the sole replica is healthy; the Pod must remain Ready. The PDB is included in
the durable application snapshot. Shell and existing targeted fixture checks
pass locally. PR workflow validation [36265900635](https://github.com/centerionware/not-k8s/actions/runs/36265900635)
passed at `2ee43f2a`, including shell syntax and the snapshot/Helm helper
checks. Branch-runtime run [36285968999](https://github.com/centerionware/not-k8s/actions/runs/36285968999)
ran this fixture at both K3s and upstream nodestore checkpoints. Both target
apiserver lanes returned 404 for `pods/eviction`, proving the endpoint was
missing before PDB behavior could be checked. The new handler and
NodeRestriction/PDB matching passed targeted quick-check
[36287345872](https://github.com/centerionware/not-k8s/actions/runs/36287345872).
Migration rerun [36287345921](https://github.com/centerionware/not-k8s/actions/runs/36287345921)
advanced beyond the PDB 429 assertion in both lanes. Run
[36288813404](https://github.com/centerionware/not-k8s/actions/runs/36288813404)
passed the source StatefulSet scale checks and target node readiness, but the
temporary ordinal-1 PV remained after claim deletion in both target lanes.
Focused `nodecontroller` quick-check
[36288813455](https://github.com/centerionware/not-k8s/actions/runs/36288813455)
passed for the first reclaim revision. CSI logs then confirmed its provisioner
will not delete a PV until its phase is `Released`; the current worktree now
sets that phase before deletion. This ordering revision is not yet verified.
No return migration or parity comparison passed. Concurrent PDB allowance
reservation is not atomic.

The same checkpoint now exercises the Deployment `/scale` subresource by
scaling nginx to two available replicas and back to one, waiting for each
controller reconciliation. Local shell syntax and helper checks passed. On
PR SHA `28d04774`, migration workflow validation
[36266043924](https://github.com/centerionware/not-k8s/actions/runs/36266043924)
and focused `nodemigrate` tests
[36266043923](https://github.com/centerionware/not-k8s/actions/runs/36266043923)
passed. The older K3s runtime job still predates this change and remained
active at 2026-09-26 19:27 UTC.

The eviction response assertion also accepts Kubernetes' `TooManyRequests`
status spelling; migration workflow validation
[36266303223](https://github.com/centerionware/not-k8s/actions/runs/36266303223)
passed at `428286d9`. The older K3s job remained active at 2026-09-26 19:31 UTC.

Commit `c3be42bc` makes built-in API 404s permanent import failures while
preserving bounded 404 retries for source-CRD custom resources. Focused
nodemigrate tests [36266754129](https://github.com/centerionware/not-k8s/actions/runs/36266754129)
and migration workflow validation
[36266754130](https://github.com/centerionware/not-k8s/actions/runs/36266754130)
passed. The behavior still needs runtime verification; it should expose the PV
request path promptly without claiming the PV migration itself is fixed. The
older K3s lane remained active at 2026-09-26 19:40 UTC.

The fixture now records the canonical listable API resource inventory at each
source, nodestore, and returned-source checkpoint, including resource kinds
with no current objects. It fails if either destination omits a resource the
source exposed, in addition to checking identities for all populated durable
resources. Local shell syntax, snapshot normalization, Helm-state, and API
inventory checks pass. On PR head
`a211413a5034dbbf342caa07e1ee739f4f239b05`, migration workflow validation
[36267290452](https://github.com/centerionware/not-k8s/actions/runs/36267290452)
and focused `nodemigrate` tests
[36267290457](https://github.com/centerionware/not-k8s/actions/runs/36267290457)
passed. Neither is runtime migration evidence. Run
[36260417450](https://github.com/centerionware/not-k8s/actions/runs/36260417450)
used older SHA `8c79470de60f288fc113db7b7b8c45da048b6ad7`: its upstream lane
failed after return-import webhook timeouts and a PV 404, while its K3s
`Run migration` step remained `in_progress` at 2026-09-26 19:51 UTC. That run
predates this inventory assertion, so runtime verification of it remains
pending. The PV failure path/root cause and K3s lane outcome remain unverified.

The source-to-nodestore comparison now checks normalized data hashes for every
retained API object in addition to ensuring all object identities remain
present. Round-trip semantic state already compared these hashes. This expands
the forward-leg assertion beyond identity-only retention; explicit fixture
resources continue to receive dedicated durable-state and behavioral checks.
Local shell syntax, API inventory positive/negative cases,
snapshot-normalization, Helm-state checks, and `git diff --check` passed at
`cccbc4afbd712ba9bdad6be783d86b4c3f983719`. Migration workflow validation
passed at branch head `e0d2247e1f75786282dd8e485a7d231d480f2018` in
[36267765013](https://github.com/centerionware/not-k8s/actions/runs/36267765013).
Runtime evidence is pending. The still-running K3s job is from an earlier SHA
and cannot exercise this change. Focused `nodemigrate` crate tests passed at
the same branch head in
[36267764895](https://github.com/centerionware/not-k8s/actions/runs/36267764895).
The release-packaging policy validation
[36267765074](https://github.com/centerionware/not-k8s/actions/runs/36267765074)
also passed without publishing a release.

Each checkpoint now exercises StatefulSet claim-template reconciliation by
scaling from ordinal 0 to ordinals 0 and 1, verifying both claims bind and
ordinal 1 can persist data, then scaling back and cleaning up the temporary
ordinal 1 claim/PV. It checks that ordinal 0 retains the same PVC UID and
payload. It also changes StatefulSet `minReadySeconds`, waits for
`observedGeneration`, and restores the original setting. This complements the
existing per-checkpoint Deployment `/scale` check. Local shell syntax and
fixture-helper checks pass at
`a936dc7e4b625ee67c49429f11a1c1778372d1c9`; PR validation and real-cluster
verification are pending. The older live K3s lane does not include this test.

Source export logs every listable API resource, the number of objects selected
for migration, and the counts/reasons for lifecycle-excluded objects or kinds.
The older skip list included legacy ServiceAccount token Secrets, which meant
their source JWTs could not authenticate after a cluster transition. The
current worktree now preserves each token Secret and obtains a destination
TokenRequest for its ServiceAccount, updating the token and cluster CA while
retaining the Secret name and other data. The integration fixture verifies its
allowed and denied API requests at source, nodestore, and returned-source
checkpoints. Focused crate and live migration verification are pending; see the
[bug tracker](NODEMIGRATE_BUGS.md). At SHA
`39890702faf4815b022bd9419472e509b6f1076e`, the focused crate tests
[36268585399](https://github.com/centerionware/not-k8s/actions/runs/36268585399)
and migration workflow validation
[36268585450](https://github.com/centerionware/not-k8s/actions/runs/36268585450)
passed. The runtime log output has not yet been verified in migration; the
active K3s lane uses an older SHA.

Commit `3f7c8873` closes a separate failure-recovery gap in reverse migration:
before reporting an API/readiness/node-replacement failure, nodemigrate now
stops the partial retained target, restores local PV payloads, and restores
the prior nodestore service stack. If the target cannot be safely stopped,
nodestore remains stopped and the error reports the recovery location. Unit
tests cover teardown ordering and source restoration behavior; focused
nodemigrate tests [36264669564](https://github.com/centerionware/not-k8s/actions/runs/36264669564)
and migration-workflow validation
[36264669587](https://github.com/centerionware/not-k8s/actions/runs/36264669587)
passed at `3f7c8873`. Runtime verification of recovery API/fixture state is
pending. The existing run `36260417450` predates this fix.

## Earlier integration attempt

Migration run [36251890971](https://github.com/centerionware/not-k8s/actions/runs/36251890971)
used SHA `16c90a9721dd1f0bbcdc1a11723d7438d173b49f`. Both K3s+Cilium and
upstream Kubernetes+Cilium lanes passed source checks, reached target
verification, and verified static hostPath and CSI data reads. Query-free SPDY
port-forwarding now works: the Gateway probe returned nginx's HTTP 200 page.
The separate Ingress probe returned HTTP 404 in both lanes. Target
`kubectl describe ingress` showed class `traefik` but no rules; raw Ingress and
IngressClass JSON was not captured, so whether the spec was lost during import
or the controller ignored it is unconfirmed. Neither lane completed its
target checkpoint, returned to the source, or compared full parity. Focused
`nodeapiserver` quick-check [36251889485](https://github.com/centerionware/not-k8s/actions/runs/36251889485)
passed. Logs:
`/tmp/nodemigrate-36251890971-artifacts/nodemigrate-{k3s,kubernetes}-36251890971/`.

Harness commit `19e1cae6` added an exact Ingress rule and IngressClass
controller check, but the first placement only exercised the source fixture.
Run [36253413938](https://github.com/centerionware/not-k8s/actions/runs/36253413938)
at that SHA passed source checks, then failed both target lanes at Ingress
routing. Its saved target JSON proves `spec.rules[].host` survived while the
embedded `http.paths` backend was dropped; the IngressClass controller was
correct. This confirms nodeapiserver's protobuf storage codec lost the
Go-embedded `IngressRuleValue` field. Commit `4984eb2e` fixes the codec,
adds a v1 round-trip regression, and moves the Ingress rule assertion into
`verify_stage` so it runs at source, target, and return checkpoints. Focused
`nodeapiserver` quick-check [36255128063](https://github.com/centerionware/not-k8s/actions/runs/36255128063)
passed at fix commit `4984eb2e`. Migration rerun
[36255128061](https://github.com/centerionware/not-k8s/actions/runs/36255128061)
is in progress; its Docker preflight passed and runtime builds are underway.
The earlier run had no target semantic checkpoint, return migration, or
parity comparison. Logs:
`/tmp/nodemigrate-36253413938-artifacts/nodemigrate-{k3s,kubernetes}-36253413938/`.

PR validation [36253405752](https://github.com/centerionware/not-k8s/actions/runs/36253405752)
passed for the earlier harness-only commit. No regular `build.yml` or full
e2e gate was run.

Earlier `nodeapiserver` quick-check
[36247102622](https://github.com/centerionware/not-k8s/actions/runs/36247102622)
passed the Job selector tests. CSI staging-path quick-check
[36246005553](https://github.com/centerionware/not-k8s/actions/runs/36246005553)
passed `nodelet`, `nodebootstrap`, and `nodemigrate`.

Branch migration [36241224151](https://github.com/centerionware/not-k8s/actions/runs/36241224151)
used SHA `606f46f624007dfd6215be26b97e54817fe020af`. Both builds, Docker
preflight, source fixtures, and destination readiness checks through CSI
readiness passed. Both lanes failed when the StatefulSet volume was staged:
the target hostpath CSI driver returned `NotFound` because the imported PV's
source volume handle was absent from its volume inventory. The source and
target handles matched, the PV topology matched the target Node, and nodelet
retries confirmed this was not a scheduling or affinity failure. Existing
nodemigrate snapshots cover `hostPath` and `local` PV paths, but not CSI
payloads or provider inventory. Migration stopped before the semantic
checkpoint; no return migration or parity passed. Artifacts:
`/tmp/nodemigrate-36241224151/`.

Branch migration [36239708922](https://github.com/centerionware/not-k8s/actions/runs/36239708922)
used SHA `64fdb6572639b53d51b5d0c8fc2ddeaac7dc09af`. Both runtime builds,
the Docker preflight, both source fixtures, and target Node/Cilium/Gateway/
Deployment/standalone Pod/CA/CSI-readiness checks passed. Both lanes confirmed
the generation fix, then failed before the semantic checkpoint because the
StatefulSet remained Pending on a CSI mount. Failure data shows the PV's node
affinity exactly matches the target Node's hostpath topology label; the earlier
scheduler conflict event therefore does not explain the final mount wait.
This run did not record nodelet/CSI logs. The branch now records source and
target PV/PVC specs and those logs for the next run. No target semantic
checkpoint, reverse migration, or data parity passed. Artifacts:
`/tmp/nodemigrate-36239708922/`.

Branch-runtime migration [36238216668](https://github.com/centerionware/not-k8s/actions/runs/36238216668)
used SHA `0254815476d1deefdb7c6465800675cceb51d1b4`. Docker preflight and
both runtime builds passed, as did both source-stage fixtures and the nodestore
target's Node, Cilium, Gateway, DaemonSet, Deployment, standalone Pod, CA, and
CSI readiness checks. The generation regression is resolved: target StatefulSet
generation and observedGeneration were both 1. The run then failed in both
lanes because the StatefulSet pod stayed Pending; its scheduler event says the
only Node had a volume node affinity conflict. The PVC/PV table displayed
Bound, but describing the claim showed Pending with zero capacity. The target
Node had its expected hostpath topology label, but this run did not log the
PV's full affinity, so the mismatch is not yet located. Harness diagnostics
now include PV YAML and target Node labels. No target semantic checkpoint,
return migration, or data parity passed. Logs:
`/tmp/nodemigrate-36238216668/`.

Diagnostic migration run [36236810283](https://github.com/centerionware/not-k8s/actions/runs/36236810283)
used SHA `b4ae3a95b89abc037ffe3d27449aad72c4eedf40`. Its five-node Docker
preflight and both builds passed; both K3s+Cilium and upstream
Kubernetes+Cilium lanes reached the same StatefulSet rollout timeout. The new
failure output showed `StatefulSet.metadata.generation` and
`status.observedGeneration` were both null, while the StatefulSet controller
had populated replicas and revision fields. The user-facing rollout command
therefore waited indefinitely for a generation that the API server never
assigned. Root cause: the nodeapiserver create-on-apply path used for migration
imports set creation time and UID but omitted server-owned generation 1;
ordinary POST creation already stamped it. The branch now applies the same
generation initialization to create-on-apply and has a focused regression.
Focused quick-check [36238216478](https://github.com/centerionware/not-k8s/actions/runs/36238216478)
passed, and the migration rerun above confirms target generation is populated.
The separate storage failure remains open. Logs:
`/tmp/nodemigrate-36236810283/`.

Run [36235423620](https://github.com/centerionware/not-k8s/actions/runs/36235423620)
used branch SHA `d177f2c664e2b661da65d43275002bf79cf71b65`. The focused
nodeapiserver quick-check [36235417698](https://github.com/centerionware/not-k8s/actions/runs/36235417698),
Docker five-node preflight, and both runtime builds passed. Both K3s+Cilium
and upstream Kubernetes+Cilium migrations reached the destination and passed
Node, Cilium, fixture DaemonSet, Deployment, GatewayClass/Gateway, standalone
Pod, CA-trust, and CSI-readiness checks. Both then timed out in
`kubectl rollout status statefulset/migration-stateful`, before the semantic
checkpoint: the rollout command waited for the StatefulSet controller to
observe the imported spec generation. At failure the ordinal Pod was Pending.
The saved log does not include the StatefulSet's generation and
`status.observedGeneration`; the follow-up diagnostic run established that
both were null. No target
semantic checkpoint, return migration, parity comparison, or full round trip
passed. Logs: `/tmp/nodemigrate-36235423620/`.

The harness now emits the StatefulSet generation/status, update strategy,
StatefulSet and Pod descriptions, and claim details on failure, and captures
the nodecontroller journal. This is diagnostic instrumentation; runtime
validation is pending. The added evidence should distinguish a controller
status gap from a Pending workload or volume issue.

Earlier GatewayClass diagnosis and history follow.

Follow-up run [36234240173](https://github.com/centerionware/not-k8s/actions/runs/36234240173)
used SHA `0a17cf9cb2e7d935afcb06e9653490aa76b1c912`. Focused nodeapiserver
quick-check [36234234075](https://github.com/centerionware/not-k8s/actions/runs/36234234075),
the Docker preflight, and both scoped builds passed. Both source lanes
completed forward migration and destination API readiness, then failed the
GatewayClass Accepted wait again. Diagnostics proved streaming-list initial
WATCH events were stamping each object's resourceVersion with the collection
snapshot revision. Traefik submitted RV `1382` for an object stored at `1049`
on K3s and `891` for one stored at `889` upstream; those stale/future writes
were correctly rejected with 409. The branch now uses each object's own
mod_revision for initial ADDED events and keeps the collection RV for the
completion bookmark, with a focused regression. Validation is pending. Logs:
`/tmp/nodemigrate-36234240173/`.

Dedicated run [36232994720](https://github.com/centerionware/not-k8s/actions/runs/36232994720)
at SHA `56f27d13d05af278706f280f69c29f3d8d1bd195` passed the Docker
preflight, scoped builds, and focused nodemigrate checks [36232987555](https://github.com/centerionware/not-k8s/actions/runs/36232987555).
Both source fixtures and forward migrations completed to destination API
readiness. In K3s, target Node, Cilium, fixture DaemonSet, nginx Deployment,
namespace CA, and CSI PVC readiness checks passed. Both lanes then failed
waiting for GatewayClass Accepted: Traefik's status PUTs received repeated
409 responses, leaving the condition Unknown. The `crd` shortcut failure and
K3s sandbox-stop timeout from the preceding run did not recur. Target semantic
checkpoints, ingress/Gateway behavior, return migration, and parity remain
unverified. A scoped nodeapiserver warning now records submitted/current
resourceVersions for this conflict; focused CI and a runtime rerun are pending.
Logs: `/tmp/nodemigrate-36232994720/`.

Dedicated run [36231779729](https://github.com/centerionware/not-k8s/actions/runs/36231779729)
used branch SHA `df31d039178266efb513eb265781f10aa498eeb2`. K3s source checks
passed and nodemigrate captured 582 objects, but `crictl stopp` timed out on a
source sandbox before destination readiness. Source rollback and protected
export retention passed. The upstream lane captured 564 objects/55 CRDs,
reported migration complete, and passed destination API readiness; Cilium and
the migration DaemonSet rolled out. Stage verification then failed because
the API does not expose kubectl's `crd` shortcut. The harness now queries
CRDs through the canonical API resource name. A utility change also allows a
stop command error only when the subsequent CRI listing proves the sandbox has
no running containers; otherwise it aborts and recovers the source. Both
changes await targeted validation. The external-CNI EndpointSlice value,
remaining workload assertions, reverse migration, and state parity remain
unverified. Logs: `/tmp/nodemigrate-36231779729/`.

Dedicated migration run [36229667964](https://github.com/centerionware/not-k8s/actions/runs/36229667964)
used branch head SHA `294a6c64`. Both utility and combined-runtime builds and
the five-node Docker isolation preflight passed. K3s accepted all 59 CRDs and
reached destination API readiness with a Ready Node. Cilium agent, Envoy, and
operator were `1/1`; the sampled live Cilium Pod's mounted service-account CA
fingerprint matched the destination API CA exactly. The remaining K3s failure
was later in workload readiness: CoreDNS Pods stayed `Running` but `0/1`, and
nodelet's journal showed it kept ordinary Pod reconciliation paused behind
the CoreDNS gate. Hostpath CSI setup consequently failed before storage or
workload assertions. CoreDNS logs report the Kubernetes plugin waiting for
API synchronization and the ready plugin remaining unready, but do not show
the underlying request error. Upstream accepted all 55 CRDs but CertificateRequest
admission failed because the cert-manager webhook Service had no endpoints.
Both lanes restored their source services and retained protected exports.
Neither reached target parity, return migration, or a full round trip. This
updates the earlier CA hypothesis: the measured K3s Cilium Pod trusted the
destination CA, but other CNI behavior and CoreDNS remain unverified. Logs:
`/tmp/nodemigrate-36229667964/nodemigrate-k3s.log` and
`/tmp/nodemigrate-36229667964/nodemigrate-kubernetes.log`.

The API endpoint is a stronger cause candidate than the probe itself: the K3s
target's `default/kubernetes` EndpointSlice still contained `127.0.0.1:6443`
while the Node InternalIP was `10.1.0.61`. `nodebootstrap` refreshed that
endpoint only for Flannel, so the external Cilium lane retained loopback. The
branch now refreshes nodeapiserver's endpoint for non-Flannel CNI using an
explicit advertise address or a detected non-loopback host IP. The focused
nodebootstrap quick-check and migration rerun are pending; the fix is not yet
verified.

Dedicated migration run [36226000144](https://github.com/centerionware/not-k8s/actions/runs/36226000144)
used SHA `edd697e94e009b7d3796de13b2ed68c076255625`. Both builds and the
five-node isolation preflight passed. Both source-stage CA checks passed, and
target snapshots repeatedly reported all namespace CA ConfigMaps matching the
destination kubeconfig. The migrations nevertheless failed applying
`CertificateRequest/migration-test-1` because the cert-manager webhook was
unreachable (HTTP 500); target Cilium and Traefik continued to reject the API
certificate with `x509: certificate signed by unknown authority`. Both source
APIs recovered and protected exports remained. No target workload/storage,
reverse-migration, or parity checkpoint passed. This shows that regenerating
the ConfigMap is insufficient if dependent Pods start before the publisher
has populated the projected CA source, or if their mounted CA remains stale.
The current fix applies source Namespaces first and ensures each destination
root CA ConfigMap matches the destination kubeconfig before applying any
other resources. This implementation has not yet passed focused CI or runtime
validation.

Dedicated migration run [36224872002](https://github.com/centerionware/not-k8s/actions/runs/36224872002)
used SHA `cf1b984a9427165401ec1fa8df386cc950539845`. Both source fixtures
completed setup, but the new `verify_stage source` CA assertion reported all
nine namespace bundles mismatched on each lane. The shell check decoded the
kubeconfig CA into command substitution, which likely removed the PEM trailing
newline; the run did not capture the compared CA bytes, so the exact mismatch
was not independently established. Crucially, this assertion failed before
`nodemigrate` ran and says nothing about the exporter change or destination
trust. The assertion now compares base64-encoded CA bytes without newline
normalization. Focused nodemigrate tests passed at this SHA in
[run 36224841930](https://github.com/centerionware/not-k8s/actions/runs/36224841930).
The next run [36226000144](https://github.com/centerionware/not-k8s/actions/runs/36226000144)
passed this comparison at source and target stages, then exposed continued
in-Pod API trust failures. No round trip passed.

Dedicated migration run [36223445443](https://github.com/centerionware/not-k8s/actions/runs/36223445443)
used SHA `9e8701291e2cd8f821f53e1e43d0bc306fd2f035`. The five-node Docker
preflight and both utility/combined-runtime builds passed. K3s reached target
API readiness, but target hostPath CSI setup failed before target workload or
reverse-migration checks. Upstream import failed on
`CertificateRequest/migration-test-1` with HTTP 500; its source API recovered
and the protected export remained. Nodeproxy started and watched Services and
EndpointSlices in both lanes; its inactive status was captured after rollback
stopped it. K3s Cilium and Traefik logs show repeated TLS failures to the
target API Service with `x509: certificate signed by unknown authority`. The
source ConfigMap export currently includes `kube-root-ca.crt`, so a stale
service-account trust bundle is a likely cause; per-namespace CA data was not
captured in this run. The migration exporter and snapshot filter now classify
that destination-managed bundle for regeneration, and the fixture checks each
namespace against the active API CA. Target and return behavior remain
unverified pending the next run.

Logs: `/tmp/nodemigrate-36223445443/nodemigrate-k3s-36223445443/nodemigrate-k3s.log`
and `/tmp/nodemigrate-36223445443/nodemigrate-kubernetes-36223445443/nodemigrate-kubernetes.log`.
The manual dispatch skipped static validation. No regular build or full e2e
ran.

Previous attempt [36222183166](https://github.com/centerionware/not-k8s/actions/runs/36222183166)
used runtime SHA `b093021677321124e8b8d318898819d90b71d927`; the proxy journal
was not captured there and the inactive status alone did not establish a
routing failure. Its logs are under `/tmp/nodemigrate-36222183166/`.

Dedicated migration run [36220533297](https://github.com/centerionware/not-k8s/actions/runs/36220533297)
used branch head `ed851766d59ac3b424c1f28e89808c5d7415ad92`. The five-node
Docker preflight and both standalone utility/combined-runtime builds passed.
K3s forward migration and destination API readiness passed, but target
hostPath CSI setup failed; the target workload checkpoint and return migration
did not run. The initial target snapshot showed the Cilium agent Ready, with
Envoy and CoreDNS running but not Ready. Failure-time diagnostics later showed
Cilium agent, Envoy, and operator `1/1`, while CoreDNS remained `0/1 Running`.
Cert-manager Pods had no phase/container status/IP and the webhook Service had
no endpoints. Upstream import failed applying
`CertificateRequest/migration-test-1`; its target cert-manager Pods likewise
had no phase/container status/IP, and its webhook Service and EndpointSlice
had no endpoints. The upstream source API recovered and the protected export
was retained. Neither lane reached workload/storage checks, reverse migration,
or semantic parity. These snapshots establish the target-time symptoms, not
their root cause.

The normalized watcher captured 10 K3s and 9 upstream snapshots, including
state changes and bounded periodic samples. This validates the diagnostic
capture correction and avoids the AGE-driven log flood from the previous run.
Logs: `/tmp/nodemigrate-36220533297-k3s.log` and
`/tmp/nodemigrate-36220533297-kubernetes.log`. No general build gate or full
e2e ran.

Branch-runtime migration [36217850294](https://github.com/centerionware/not-k8s/actions/runs/36217850294)
used head SHA `d7b65846f55f24e75fd56ca54d107f5bad8b5511`. Both scoped utility
and combined-runtime builds and the five-node Docker preflight passed; both
migration lanes failed during forward import. K3s accepted the CiliumNode
update with HTTP 200, but importing `CertificateRequest/migration-test-1`
failed because the cert-manager admission webhook was unreachable (HTTP 500).
The CoreDNS and Cilium pod diagnostics were collected after nodemigrate restored
the source services; they are post-rollback source state, not target state, and
cannot explain the webhook failure. A target-state watcher now samples CoreDNS,
Cilium, cert-manager pods, and webhook endpoints while the forward migration
is running, but its runtime validation is pending. Both lanes restored source
service and retained protected exports.
Neither reached target workload/storage assertions, reverse migration, or
semantic parity. No regular build or full e2e gate ran. Logs:
`/tmp/nodemigrate-36217850294-k3s.log` and
`/tmp/nodemigrate-36217850294-kubernetes.log`.

Previous branch-runtime migration [36216429427](https://github.com/centerionware/not-k8s/actions/runs/36216429427)
used head SHA `5a2a2630dc70ca27b5d0ed642a11319547f50869`. Both scoped builds
and the Docker preflight passed. K3s source assertions, forward transfer, and
target API readiness passed; Cilium agent, Envoy, and operator reached `1/1`,
and the CiliumNode update returned HTTP 200. Target hostpath CSI setup failed
with three CoreDNS pods `Running` but `0/1` Ready and most other target pods
`Unknown` with no IP. CoreDNS descriptions and logs were not captured. Upstream
failed importing the same CertificateRequest through the unreachable
cert-manager webhook. Both lanes restored source service and retained the
protected export. Neither reached workload/storage checks, reverse migration,
or parity. Logs: `/tmp/nodemigrate-36216429427-k3s.log` and
`/tmp/nodemigrate-36216429427-kubernetes.log`.

Branch-runtime migration [36214776875](https://github.com/centerionware/not-k8s/actions/runs/36214776875)
used runtime SHA `70d1a049` (workflow checkout also contained docs-only
commit `ef20bbc2`). Both utility/runtime builds and the five-node Docker
preflight passed. In K3s, replacement CSI Pods again used the target
`/var/lib/nodelet` paths, confirming the old terminating-Pod blockage did not
recur. Cilium started, then exhausted ten updates to
`CiliumNode/runnervmtr4k5` with HTTP 409 “object has been modified” and exited
fatally; the `agent-not-ready` taint remained and replacement CSI Pods could
not schedule. The nodelet HTTP probe fix passed its focused quick-check, but
the agent never reached stable Ready, so its runtime effect remains
unverified. Upstream again rolled back after the cert-manager webhook could
not be reached for `CertificateRequest/migration-test-1` (HTTP 500). Both
source services recovered and protected exports remained. No target workload,
return-migration, or parity checkpoint passed. General build/e2e gates were
not run. The saved API server journal tail began after the CiliumNode failure
window, so it did not establish whether the 409 came from a stale submitted
resourceVersion or a storage compare-and-swap loss. The integration harness is
being updated to retain matching CiliumNode journal entries from migration
start. Logs: `/tmp/nodemigrate-36214776875-k3s.log` and
`/tmp/nodemigrate-36214776875-kubernetes.log`.

Branch-runtime migration [36212326763](https://github.com/centerionware/not-k8s/actions/runs/36212326763)
used SHA `b2c4f8824faf2c490aa13bf61f0a7c4509a27d05`. Both scoped builds and
five-node Docker preflight passed. In the K3s lane, the nodelet startup-gate
teardown change cleared the prior stale CSI Pod blockage: replacement CSI Pods
had target `/var/lib/nodelet` paths and were newly created. They remained
Pending because the node retained Cilium's
`node.cilium.io/agent-not-ready:NoSchedule` taint. The Cilium agent container
was Running but not Ready; `cilium status` showed 42/44 controllers healthy,
all 85 reported modules OK, and 1/1 node reachable. Inspection found nodelet
ignored the probes' `httpGet.host: 127.0.0.1` and configured headers, instead
connecting to the Pod IP; the Cilium health server bound to loopback. The
operator's repeated liveness failures and agent readiness failure are
consistent with this confirmed probe defect. The fix and focused request tests
passed nodelet quick-check [36214612947](https://github.com/centerionware/not-k8s/actions/runs/36214612947).
Migration rerun [36214776875](https://github.com/centerionware/not-k8s/actions/runs/36214776875)
then exposed a fatal CiliumNode 409 before the agent reached stable Ready;
runtime probe behavior remains unverified. Upstream again rolled back
after the cert-manager webhook could not be reached for
`CertificateRequest/migration-test-1` and the API returned HTTP 500. Source
recovery and protected-export retention passed in both lanes. Neither lane
reached target workload, return-migration, or state-parity checks. The regular
build gate and full e2e were not run. Logs:
`/tmp/nodemigrate-36212326763-k3s.log` and
`/tmp/nodemigrate-36212326763-kubernetes.log`.

Branch-runtime migration [36211105237](https://github.com/centerionware/not-k8s/actions/runs/36211105237)
used diagnostic SHA `0bc658e76b58f4f29ce2d69c310826fab8fe9ea9`. Both scoped
builds and five-node Docker preflight passed. In K3s, all six Cilium init
containers completed, but the nodelet stayed in `wait_for_coredns()` through
the hostpath CSI timeout. Its journal still showed Cilium being reconciled
"while CoreDNS is gated" at 02:35:39Z and showed no terminating-Pod watch
acceptance. This confirms the gate was not processing the CSI Pods' pending
deletions. A fix now starts teardown for locally assigned terminating Pods
during the gate; its local-versus-remote regression passed focused nodelet
quick-check [36211663128](https://github.com/centerionware/not-k8s/actions/runs/36211663128).
Runtime verification is pending. Upstream again failed CertificateRequest
restoration because the target cert-manager webhook was unreachable; rollback
restored the source API and retained the protected export. Neither lane
reached workload checks, reverse migration, or semantic parity. The regular
build gate and full e2e were not run. Logs:
`/tmp/nodemigrate-36211105237-k3s.log` and
`/tmp/nodemigrate-36211105237-kubernetes.log`.

Branch-runtime migration [36195385046](https://github.com/centerionware/not-k8s/actions/runs/36195385046)
used SHA `6037e67e1f4e0a9d8d365d5ce8653c572c29ed72`. K3s source checks and
all 59 CRD imports passed, followed by destination API readiness. Cilium's
`mount-bpf-fs` stop event included Pod metadata, and later CRI inspection
showed a successful exit, but the Pod status remained Running and no next init
container was created; target CNI and CSI checks stopped there. Upstream
accepted all 55 CRDs, then lost destination Cilium networking and failed
CertificateRequest webhook admission. Both lanes restored the source and
retained protected exports. Neither lane reached target workloads, reverse
migration, or parity. The CiliumNode conflict diagnostic was not exercised.
Direct CoreDNS-gate reconcile and CRI init-state diagnostics were added in
`9974e763`; focused nodelet quick-check passed in
[36197970777](https://github.com/centerionware/not-k8s/actions/runs/36197970777).
Diagnostic branch-runtime rerun
[36197970992](https://github.com/centerionware/not-k8s/actions/runs/36197970992)
completed with both migration steps failed after their scoped builds passed;
Docker preflight passed. GitHub job metadata confirms the failing step was
`Run migration` in both lanes (K3s ran 21m18s; upstream 16m03s). The operation
is now classified from the captured logs. In K3s, Cilium's `mount-bpf-fs`
container logged that bpffs was mounted, then CRI no longer had a live task;
the Pod stayed at `Init:3/6`, and CoreDNS sandbox setup later failed when
`cilium-cni` returned `signal: killed`. Independently, the target
ResourceQuota controller entered a status-write loop: 6,376–8,366
`resourcequotas` range calls per 30 seconds and 1,167 PATCH audit records for
`migration-quota/status` in about 1.2 seconds. The fixture's quota includes
`requests.storage`, which this controller does not calculate; comparing the
whole `status.used` map to its partial result made the merge PATCH repeat
forever. Upstream showed the same quota range storm and failed
CertificateRequest import because the cert-manager webhook was unreachable
after CNI failed. This confirms the quota defect but does not yet prove it
caused the Cilium task or CNI process to be killed. Both lanes restored the
source and retained protected exports. Neither lane reached a target, return,
or parity checkpoint. Logs: `/tmp/nodemigrate-361979-k3s.log` and
`/tmp/nodemigrate-361979-kubernetes.log`.

Branch-runtime migration [36192756836](https://github.com/centerionware/not-k8s/actions/runs/36192756836)
used SHA `c70f53023a4a48c04b0d7b85d4dda3b6388a2b50`. Both scoped runtime and
nodemigrate builds passed, as did five-node Docker isolation. K3s source
checks passed and all 59 captured CRDs imported; the target API became ready,
but Cilium exhausted ten `CiliumNode` update retries with HTTP 409 before the
target workload/CSI checkpoint. No reverse migration or parity comparison
passed. The server diagnostics did not identify the conflict path in this run;
the status-subresource stale-resourceVersion path now has separate logging and
needs a focused check and migration rerun. Upstream source checks passed and
all 55 CRDs imported, but CertificateRequest admission returned HTTP 500 after
the Cilium CNI plugin/agent disappeared. Rollback restored the source and
retained the protected export. Neither lane passed the target, reverse, or
round-trip checkpoints. Logs:
`/tmp/nodemigrate-36192756836/{k3s,kubernetes}/`.

Branch-runtime migration [36183868918](https://github.com/centerionware/not-k8s/actions/runs/36183868918)
used code SHA `64baa5ab795bfd71fed1ffe193eb08e974e5345d`. Nodemigrate and
combined-runtime builds passed, as did the five-node Docker isolation
preflight. K3s source checks passed; nodemigrate captured 582 API objects and
59 CRDs, imported every CRD, and reached destination API readiness. Destination
Cilium then stalled: CRI reported `apply-sysctl-overwrites` exited with code 0
at 20:26:53Z, while Pod status still reported it Running at 20:28:17Z. The
following `mount-bpf-fs` init container never started; target workload/CSI
checks, reverse migration, and parity comparison did not run. Upstream source
checks passed and all 55 CRDs were imported before CertificateRequest admission
failed with HTTP 500 because the destination cert-manager webhook was
unreachable while CNI was unavailable. Source rollback and protected-export
retention passed. The new CRI process snapshot confirms the init task had
exited, but did not capture the CRI event payload; nodelet's event handler has
a missing-metadata drop path that is now being fixed and tested. Both required
merge gates remain open. Full logs are at
`/tmp/nodemigrate-36183868918/nodemigrate-{k3s,kubernetes}-36183868918/`.

The previous focused nodemigrate quick-check passed at SHA
`29b6b7de9575e7cf0d43ca60becba868373d86f3` in
[36168559894](https://github.com/centerionware/not-k8s/actions/runs/36168559894);
it does not include the new detector fix.

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
in both directions: K3s → not-k8s → K3s and upstream Kubernetes → not-k8s →
upstream Kubernetes. Check identity, spec, data, status, ownership, and
observable behavior at the initial source, not-k8s target, and returned-source
checkpoints. Rows marked partial describe current fixture setup only; no row
is complete until the same object/state and behavior assertions pass through
a full round trip. The named fixtures below extend the existing migration
tests; they do not replace or narrow any existing coverage.

| Resource group | Required migration and behavior checks | Current fixture status |
| --- | --- | --- |
| Configuration and identity | Namespaces, ConfigMaps (including binary data), Secrets, ServiceAccounts, Roles, ClusterRoles, RoleBindings, ClusterRoleBindings, ResourceQuotas, LimitRanges, and PriorityClasses; verify identity/data and allowed plus denied requests using real service-account credentials. | The fixture includes text/binary ConfigMaps, an immutable ConfigMap mounted into and read by a standalone Pod, a Secret consumed through Pod environment, namespaced and cluster-scoped RBAC, and real-token Jobs for allowed ConfigMap/Node reads and denied Secret reads. ResourceQuota, LimitRange, PriorityClass, immutable state, mounted data, and namespace CA bundles are checked at each stage. The upstream lane in run 36335580680 passed source and nodestore checks, including real-token RBAC; return-stage results are unverified; K3s was cancelled at the workflow timeout. Broader RBAC/quota cases remain pending. |
| Workload controllers | Deployments, ReplicaSets, ReplicationControllers, StatefulSets and `volumeClaimTemplates`, DaemonSets on every node, Jobs, CronJobs, standalone Pods, and PodTemplates; verify selectors, templates, rollout/revision history, replica/readiness counts, job execution, schedule, and unique application data at each checkpoint. | The fixture includes an nginx Deployment, a StatefulSet backed by a CSI claim template with a seeded payload, an all-node DaemonSet, a completed Job, a manually triggered CronJob, and a standalone data-seed Pod. It now also creates a PodTemplate and a one-replica ReplicationController and checks the template contents plus a Ready controller-owned Pod at each checkpoint. These latest assertions have not run in CI. Prior upstream source and nodestore checks in run 36335580680 passed for the then-current workload fixture, including StatefulSet scale/PVC/data; return-stage results are unverified; K3s was cancelled at the workflow timeout. Multi-node daemonset coverage and full round-trip parity remain unverified. |
| Helm-managed applications | Helm charts/releases and release records (name, namespace, chart/version, values, revision, manifest), plus all chart-managed resources; verify `helm list`, release inspection, workload health, and a safe follow-up Helm operation after each cutover. | Traefik, cert-manager, and Cilium are installed by Helm. Source and nodestore assertions in the upstream lane of run 36335580680 passed release inspection and version-pinned server-side dry-run upgrades for Traefik and Cilium. Returned-source release parity and safe follow-up operation remain unverified; K3s was cancelled at the workflow timeout. |
| Service networking and ingress | Services, user-managed Endpoints and EndpointSlices, controller-generated endpoints, IngressClasses and Ingresses, NetworkPolicies, and Gateway API `GatewayClass`, `Gateway`, `HTTPRoute`, `GRPCRoute`, `TCPRoute`, `TLSRoute`, and `UDPRoute` where supported; verify DNS, service reachability, policy allow/deny, HTTP/TLS routing, and certificate use. | The fixture creates a selectorless Service with user-managed Endpoints and EndpointSlice objects and checks them at each checkpoint; the exporter skips only recognized Kubernetes-generated endpoint records. Source and nodestore route, Gateway, certificate, and allowed/denied NetworkPolicy checks passed in the upstream lane of run 36335580680. Return-stage behavior failed during webhook-backed API import; broader route protocols remain pending; K3s was cancelled at the workflow timeout. |
| Persistent storage | PVs, PVCs, StorageClasses, static hostPath/local volumes, dynamic CSI volumes, CSIDrivers, CSINodes, VolumeAttachments where applicable, VolumeSnapshotClasses, VolumeSnapshots, snapshot contents, and StatefulSet claim templates; verify binding, topology, provider configuration/credentials, attachment behavior, and unique payload data. | The upstream lane in run 36335580680 passed source and nodestore checks for the static hostPath PV/PVC, dynamic hostPath CSI claims, and StatefulSet claim-template PV/PVC payloads. No PV, PVC, or backing volume deletion was reported. Return-stage checks, multi-node provider attachment, and other provisioner classes remain unverified; K3s was cancelled at the workflow timeout. |
| CRDs and operator resources | CRDs plus representative custom resources and durable operator state, including Cilium configuration/policy, cert-manager Issuers, ClusterIssuers, Certificates and CertificateRequests, and Gateway API resources; verify discovery, version conversion, reconciliation, status, and resulting Secrets/routes. | Cilium, cert-manager, and Gateway API CRDs/custom resources are sampled and Gateway API resources have route status/behavior checks. The fixture now adds a user-defined cluster-scoped MigrationRecord CRD with served v1alpha1/v1 versions plus a durable custom resource; every stage checks Established/discovery and reads the same object through v1alpha1. The custom resource fingerprint also participates in the all-discovered API inventory. Static validation passed; target/return runtime behavior is pending. Broader operator-specific resources and successful target reconciliation remain unverified. |
| Policies, admission, and scheduling | PodDisruptionBudgets, autoscalers, NetworkPolicies, validating/mutating webhook configurations, admission policies, APIService registrations where present, RuntimeClasses, node selectors/affinity, tolerations, topology spread, and other supported constraints; verify stored configuration and observable decisions. | The upstream source and nodestore checks in run 36335580680 exercised the nginx PDB eviction denial, ingress NetworkPolicy allow/deny probes, and cert-manager webhook-backed certificate behavior. The return import could not reach the cert-manager webhook Service. HPA behavior, admission decisions beyond installed cert-manager, APIService behavior, RuntimeClass, expanded scheduling constraints, and the K3s outcome remain unverified. |
| Full API inventory | Enumerate source discovery and every listable API resource and subresource. In addition to all fixtures above, cover Nodes and node status/configuration, Events and Leases, PodDisruptionBudgets, HPA and other autoscaling APIs, Pod Security admission, admission webhook configurations and CA bundles, ValidatingAdmissionPolicies and bindings, APIService registrations, RuntimeClasses, Priority and Fairness configuration, Dynamic Resource Allocation classes/claims/slices where served, EndpointSlices, certificate requests/signing requests, service-account token and authorization review behavior, and storage expansion/snapshot/attachment resources. Include init and ephemeral containers, rollout history, owner references/finalizers, conversion and admission webhooks, service-account identity, Secret references, and status fields that affect controller recovery. Compare identity, spec, data, status, ownership, and observable behavior at source, target, and returned-source checkpoints in both migration directions. Every discovered kind must migrate or be regenerated only when its owning Kubernetes lifecycle necessarily recreates it; do not silently omit kinds because they are uncommon, transient, or add-on-owned. | The harness captures a source object inventory and semantic checkpoint, but completeness and bidirectional migration parity have not passed a full round trip. The additional API families and lifecycle behaviors in this row are acceptance requirements, not yet verified coverage. |

The list above is a minimum and supplements resource kinds already present in
the fixture. This explicitly includes ConfigMaps, CRDs and their custom
resources, StatefulSets, Deployments, Helm chart releases and chart-managed
objects, CronJobs, DaemonSets, Ingresses, Gateway API objects, and RBAC.
Every named and previously covered resource group must be tested for
migration and behavior at all three checkpoints in both directions; a source
setup, successful API import, or one-way migration is not a passing test. Any
additional API kind, add-on, data path, or controller behavior found in source
discovery or real workloads must be added to this matrix and exercised unless
its lifecycle is explicitly documented. If a kind cannot be preserved
directly, its required regeneration and equivalent behavior must be asserted.

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

### Persistent-volume deletion invariant

Migration must preserve source PVs, PVCs, bindings, reclaim policies, backing
volumes, and payloads. Source review of `crates/nodemigrate/src` found one
Kubernetes API delete operation: it removes a stale same-name `Node` only after
an expected-UID precondition. The migration harness deletes temporary Jobs
and Pods but does not delete PVCs or PVs. The PV reclaim behavior recorded in
the bug tracker is a separate `nodecontroller` test that deliberately deletes
its disposable PVC to exercise the `Delete` reclaim policy; it is not a
migration action. No migration or fixture cleanup may delete a source PV/PVC
or backing volume.

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
| 2026-09-26 | `e49d4b602e5a5d2d87b1d2a6c8150148eaa2542a` | Docker-only kubeadm 1.35 five-node preflight | Failed in `Build node image` before containers, kubeadm setup, or Cilium checks. `curl` could not read `/etc/ssl/certs/ca-certificates.crt` while fetching the Kubernetes signing key; GPG consequently received no key data. The Dockerfile now installs `ca-certificates` explicitly. | [Run 36269422051](https://github.com/centerionware/not-k8s/actions/runs/36269422051) |
| Worktree after `e49d4b60` | — | Docker image CA bundle fix | Added explicit `ca-certificates` installation to the five-node image. Docker-only rerun pending; no cluster simulation or migration evidence yet. | Dedicated `docker_only=true` run pending |
| 2026-09-26 | `4c5af2adb01fb876409462f3054de1f38923f8e8` | Docker-only kubeadm 1.35 five-node preflight | Image built successfully; five node containers passed systemd readiness and isolation checks for network/mount namespaces, machine identity, CRI, BPF, storage, and peer connectivity. Probe then failed at `modprobe overlay` because `modprobe` was missing. The Dockerfile now installs `kmod`; kubeadm/Cilium checks remain pending. | [Run 36269756740](https://github.com/centerionware/not-k8s/actions/runs/36269756740); log `/tmp/nodemigrate-36269756740/docker-preflight.log` |
| 2026-09-26 | `4e008e5ad32de1c5032273d8712dc5b9b172c973` | Docker-only kubeadm 1.35 five-node preflight | Image built and all five containers passed systemd, isolation, CRI, BPF, storage, and peer-network checks. Probe then failed because `modprobe` could not find `overlay` under the container's `/lib/modules/6.17.0-1022-azure`; those are runner-host kernel modules. The harness now loads them on the Docker host before launching the privileged containers and removes the unnecessary image `kmod` package. | [Run 36269896199](https://github.com/centerionware/not-k8s/actions/runs/36269896199); log `/tmp/nodemigrate-36269896199/docker-preflight.log` |
| Worktree after `4e008e5a` | — | Docker preflight host-kernel module setup | Moved `overlay` and `br_netfilter` module loading to the Docker host, whose kernel is shared by all privileged node containers. Rerun pending. | Dedicated `docker_only=true` run pending |
| 2026-09-26 | `2f35443e3b5b6e141aa03b47bf5e0fc041fe0d0d` | Docker-only kubeadm 1.35 five-node preflight | Host module loading and all five container isolation/CRI/BPF/storage checks passed. Setup then exited 32, but the script suppressed `sysctl --system` output, so the failing command could not be identified. The harness now emits step markers and sysctl output for diagnosis. | [Run 36270087374](https://github.com/centerionware/not-k8s/actions/runs/36270087374); log `/tmp/nodemigrate-36270087374/docker-preflight.log` |
| Worktree after `2f35443e` | — | Docker preflight prerequisite diagnostics | Added per-command markers and restored `sysctl --system` output so the next probe identifies which kubeadm prerequisite command fails. | Dedicated `docker_only=true` run pending |
| 2026-09-26 | `b9b07dde4dea42fae7e41c9cd3c81b5a2b91ad9e` | Docker-only kubeadm 1.35 five-node preflight | Host module loading, sysctl setup, and all five container isolation/CRI/BPF/storage checks passed. `swapoff -a` returned 32 in `cp-1`. The subsequent runner-level check found a 3 GiB `/swapfile` with zero used. | [Run 36270240134](https://github.com/centerionware/not-k8s/actions/runs/36270240134); log `/tmp/nodemigrate-36270240134/docker-preflight.log` |
| 2026-09-26 | `69a6b83b4bb8a8cba520d1d48f3f3ad455f4cf3d` | Docker-only kubeadm 1.35 five-node preflight | Host modules, sysctls, and five-container isolation checks passed. The runner swap check correctly stopped setup because a 3 GiB `/swapfile` was active (zero used); no cluster setup ran. The harness now records memory/swap and disables swap on the ephemeral CI host before kubelet setup, then verifies swap is off. | [Run 36270390355](https://github.com/centerionware/not-k8s/actions/runs/36270390355); log `/tmp/nodemigrate-36270390355/docker-preflight.log` |
| Worktree after `69a6b83b` | — | Docker preflight ephemeral-host swap setup | Added runner memory/swap diagnostics, disables swap on the disposable Actions host, verifies the swap is off, then starts isolated nodes. | Dedicated `docker_only=true` run pending |
| 2026-09-26 | `8e50641364b2803ab38ca96d3538dbc6d6d09805` | Docker-only kubeadm 1.35 five-node preflight | Runner memory/swap diagnostics showed 15 GiB RAM and a 3 GiB unused swapfile. Host swap was disabled and verified; host modules, sysctls, and five container isolation checks passed. Setup then failed because `/etc/containerd` was absent. The script now creates that directory before writing the containerd config. | [Run 36270521782](https://github.com/centerionware/not-k8s/actions/runs/36270521782); log `/tmp/nodemigrate-36270521782/docker-preflight.log` |
| Worktree after `8e506413` | — | Docker preflight containerd config path | Creates `/etc/containerd` before generating the containerd configuration. | Dedicated `docker_only=true` run pending |
| 2026-09-26 | `4d1a152ddd62ffe8aa0c602253c35381a34042aa` | Docker-only kubeadm 1.35 five-node preflight | Host swap was disabled, host modules loaded, and all five isolated containers passed systemd/CRI/BPF/storage/network checks. Sysctls and containerd config then passed; `kubeadm init` failed SystemVerification because `modprobe` was missing in the node image. The Dockerfile restores `kmod`, and node containers now mount host `/lib/modules` read-only. | [Run 36270640042](https://github.com/centerionware/not-k8s/actions/runs/36270640042); log `/tmp/nodemigrate-36270640042/docker-preflight.log` |
| Worktree after `4d1a152d` | — | Docker preflight kubeadm kernel config | Restored `kmod` and made runner kernel modules visible read-only inside the containers for kubeadm SystemVerification. | Dedicated `docker_only=true` run pending |
| 2026-09-26 | `2ccff8355f556ebf3030f3b80c21c09eaa212ae5` | Docker-only kubeadm 1.35 five-node preflight | Host swap handling, kernel modules, sysctls, containerd setup, and five-node isolation checks passed. `kubeadm init` then failed SystemVerification because the runner has no `configs` module for kernel `6.17.0-1022-azure`. The probe now requires the matching host `/boot/config-$(uname -r)` and mounts `/boot` read-only into each node for kernel-config inspection. | [Run 36270787465](https://github.com/centerionware/not-k8s/actions/runs/36270787465); log `/tmp/nodemigrate-36270787465/docker-preflight.log` |
| Worktree after `2ccff835` | — | Docker preflight host kernel config | Checks and mounts the Docker host's exact kernel config read-only so kubeadm can inspect the shared kernel without relying on an unavailable `configs` module. | Dedicated `docker_only=true` run pending |
| 2026-09-26 | `fe36cc6ab38783eba819fb2a3f7f07ea17df5278` | Docker-only kubeadm 1.35 five-node preflight | Kubeadm passed SystemVerification and generated control-plane certs/manifests. API server bootstrap then timed out while creating the admin ClusterRoleBinding. The prior failure path deleted containers without collecting runtime state; the harness now captures kubelet/containerd journals, CRI tasks/containers, and node-container logs across all five nodes before cleanup. | [Run 36271060218](https://github.com/centerionware/not-k8s/actions/runs/36271060218); log `/tmp/nodemigrate-36271060218/docker-preflight.log` |
| Worktree after `fe36cc6a` | — | Docker preflight kubeadm failure diagnostics | Collects all node runtime state before cleanup if the first control-plane `kubeadm init` fails. | Dedicated `docker_only=true` run pending |
| 2026-09-26 | `80160e9c3e455907f4b02cf1b297637ff0c999e9` | Docker-only kubeadm 1.35 five-node preflight | Kubeadm passed preflight but the API server never became available. Captured containerd/kubelet logs showed OverlayFS rootfs mounts for static Pods repeatedly failed with `invalid argument`, preventing etcd and control-plane containers from starting. The node containerd configuration now selects the `native` CRI snapshotter to avoid nested OverlayFS. | [Run 36271304303](https://github.com/centerionware/not-k8s/actions/runs/36271304303); log `/tmp/nodemigrate-36271304303/docker-preflight.log` |
| Worktree after `80160e9c` | — | Docker preflight nested OverlayFS | Sets the node CRI snapshotter to `native` and verifies the generated containerd config contains that setting before restart. | Dedicated `docker_only=true` run pending |
| 2026-09-26 | `a6e6f831bad820f427cf8463e72148842072a657` | Docker-only kubeadm 1.35 five-node preflight | Five containers passed isolation checks, but the probe failed in the inner setup shell with an unmatched quote in the snapshotter sed command. The command now uses simpler quoting for exact `overlayfs`→`native` replacement. | [Run 36271561812](https://github.com/centerionware/not-k8s/actions/runs/36271561812); log `/tmp/nodemigrate-36271561812/docker-preflight.log` |
| Worktree after `a6e6f831` | — | Docker preflight snapshotter command quoting | Simplified the exact snapshotter replacement and checks for `snapshotter = "native"` in the generated config before restarting containerd. | Dedicated `docker_only=true` run pending |
| 2026-09-26 | `243b064e1d6beeb802bd5bc93e53492828865bce` | Docker-only kubeadm 1.35 five-node preflight | Five containers passed isolation checks; configuration failed before containerd restart because the generated snapshotter setting did not match the exact replacement/assertion. The harness now replaces `overlayfs` only on snapshotter-setting lines and prints matching config when its assertion fails. | [Run 36271711545](https://github.com/centerionware/not-k8s/actions/runs/36271711545); log `/tmp/nodemigrate-36271711545/docker-preflight.log` |
| Worktree after `243b064e` | — | Docker preflight TOML quote handling | Makes the native snapshotter edit independent of whether generated TOML uses single or double quotes. | Dedicated `docker_only=true` run pending |
| Worktree after `7c5f2935` | — | API identity and system add-on lifecycle checks | Object identity now uses API group, kind, namespace, and name so served-version conversion does not look like object loss. The two missing K3s system add-on ReplicaSets are classified as regenerated children of `kube-system` CoreDNS/local-path-provisioner Deployments; each available Deployment must be rolled out and have a current ReplicaSet. User fixture ReplicaSet and ControllerRevision history remains asserted. Snapshot filter, script syntax, and diff checks passed; migration rerun pending. | Dedicated migration workflow pending |
| 2026-09-26 | `7c5f2935146647c045f3f27d0d3dc9317ebfc9ed` | Branch-runtime K3s+Cilium and upstream Kubernetes+Cilium | Both source and target (`nodestore`) stages passed. The retention assertion found two K3s system add-on ReplicaSets replaced by their Deployment controllers, and one ClusterTrustBundle surfaced via a different served API version. Fixture Deployment/StatefulSet history checks passed. The next harness update classifies those system add-on ReplicaSets as regenerated and compares API group identity across versions. | [Run 36257337049](https://github.com/centerionware/not-k8s/actions/runs/36257337049); artifacts `/tmp/nodemigrate-36257337049-artifacts/` |
| 2026-09-26 | `7c5f2935146647c045f3f27d0d3dc9317ebfc9ed` | Latest regular release v0.8.0 against K3s+Cilium and upstream Kubernetes+Cilium | Utility builds and source fixtures passed. The K3s target still advertised API endpoint `127.0.0.1`, preventing target Cilium and cert-manager pods from starting. Upstream target failed CertificateRequest admission with the unreachable cert-manager webhook and CSR import with the known `ExtraValue` codec failure. Both sources recovered and retained protected exports; target workload checks and parity were not reached. | [Run 36256671367](https://github.com/centerionware/not-k8s/actions/runs/36256671367); artifacts `/tmp/nodemigrate-36256671367-artifacts/` |
| 2026-09-26 | `4984eb2e8a285f284c8bbbb9581362cdd61d0f6c` | Branch-runtime K3s+Cilium and upstream Kubernetes+Cilium | Both target stages passed, including Ingress behavior and the fixture's workload/storage checks. Both lanes then failed an exact hash comparison of all discovered API objects, which conflated newly generated destination runtime objects and API/controller updates with durable source state. No reverse migration ran. The comparison now checks source object identity retention and directly compares stable fixture snapshots; rerun pending. | [Run 36255128061](https://github.com/centerionware/not-k8s/actions/runs/36255128061); logs `/tmp/nodemigrate-36255128061-artifacts/` |
| 2026-09-26 | `f86fad5622d48582c24531df4b88368096d632d9` | K3s+Cilium and upstream Kubernetes+Cilium against regular release v0.8.0 | Utility builds, five-node Docker preflight, and K3s source fixture passed; K3s export captured 582 objects/59 CRDs. v0.8.0 rejected three Gateway API CRDs for unsupported CEL `matches`. Target snapshots showed no schedulable Nodes, no running Cilium/cert-manager Pods, and no webhook endpoints; mounted-CA probe could not exec because nodelet port 10250 refused connections. Upstream again failed CertificateRequest/CSR import with HTTP 500 and hit the same CRD CEL failures. Both source distributions recovered and protected exports were retained. No target workload, reverse migration, or parity checkpoint passed. | [Run 36228920539](https://github.com/centerionware/not-k8s/actions/runs/36228920539); artifacts `/tmp/nodemigrate-36228920539/` |
| 2026-09-26 | `d92abf28397e143c266c115cf2deb7d694f23374` | K3s+Cilium and upstream Kubernetes+Cilium against regular release v0.8.0 | Release fetch, nodemigrate builds, Docker isolation, and both source CA checks passed. K3s still logged Cilium/Traefik API TLS `unknown authority` despite destination CA ConfigMaps matching the active kubeconfig. Upstream failed CertificateRequest and CSR imports with HTTP 500 and had three Gateway API CRDs rejected because v0.8.0 CEL does not recognize `matches`. Rollback restored both sources and retained protected exports. No target workload, reverse migration, or parity checkpoint passed. | [Run 36228127861](https://github.com/centerionware/not-k8s/actions/runs/36228127861); logs `/tmp/nodemigrate-36228127861/` |
| 2026-09-26 | `d7b65846f55f24e75fd56ca54d107f5bad8b5511` | K3s+Cilium and upstream+Cilium branch-runtime migration | Both nodemigrate/combined-runtime builds and the five-node Docker preflight passed. Both forward migrations failed on `CertificateRequest/migration-test-1` because the cert-manager webhook was unreachable (HTTP 500). K3s CiliumNode update returned HTTP 200. The failure bundle's CoreDNS/Cilium pod state was collected after rollback and describes restored source state, not the target. No workload, reverse-migration, or parity checkpoint passed. | [Run 36217850294](https://github.com/centerionware/not-k8s/actions/runs/36217850294); logs `/tmp/nodemigrate-36217850294-{k3s,kubernetes}.log` |
| Worktree after `d7b65846` | — | Forward-migration target diagnostics | Added a watcher to sample target CoreDNS/Cilium/cert-manager pods, logs, and webhook endpoints while nodemigrate is running, before rollback stops the target services. `bash -n` and `git diff --check` passed; migration runtime validation is pending. | Dedicated migration workflow pending |
| 2026-09-26 | `5a2a2630dc70ca27b5d0ed642a11319547f50869` | K3s+Cilium and upstream+Cilium branch-runtime migration | Both scoped builds and Docker preflight passed. K3s reached target API readiness, but hostpath CSI setup failed with three CoreDNS pods `Running` but `0/1` Ready and other target pods `Unknown`; no CoreDNS logs were captured. Upstream import failed at the cert-manager webhook. Source recovery passed; target workload checks, reverse migration, and parity did not run. | [Run 36216429427](https://github.com/centerionware/not-k8s/actions/runs/36216429427); logs `/tmp/nodemigrate-36216429427-{k3s,kubernetes}.log` |
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
| 2026-09-25 | `85ed67ed2d4b5540b9cbe3268b061955d946a98b` | K3s+Cilium round trip against v0.8.0 | Source fixture passed; nodemigrate captured 571 objects/58 CRDs and selected `/etc/cni/net.d` containing Cilium's config. v0.8.0 rejected the three Gateway API CRDs due to missing CEL `matches`; rollback restored K3s and retained the export. No target checkpoint, reverse migration, or semantic parity comparison ran. | [Run 36174946764](https://github.com/centerionware/not-k8s/actions/runs/36174946764); `/tmp/nodemigrate-36174946764/k3s.log` |
| 2026-09-25 | `85ed67ed2d4b5540b9cbe3268b061955d946a98b` | Upstream Kubernetes+Cilium round trip against v0.8.0 | Source fixture passed; nodemigrate captured 551 objects/54 CRDs. The release destination rejected the three Gateway API CRDs for missing CEL `matches`; CertificateRequest admission and CSR import returned HTTP 500. Rollback restored the source and retained the export. No target checkpoint, reverse migration, or semantic parity comparison ran. | [Run 36174946764](https://github.com/centerionware/not-k8s/actions/runs/36174946764); `/tmp/nodemigrate-36174946764/kubernetes.log` |
| 2026-09-25 | `77e023f1349a6345f5a2bccd1e3a838887b3bbc5` | Branch-runtime K3s+Cilium migration | Source fixture passed; migration imported 58 CRDs and destination API readiness passed. Cilium's `mount-bpf-fs` init printed the mounted bpffs line but was still reported CRI-Running over a minute later; subsequent Cilium init containers never started and the node stayed CNI-unready. No target fixture checkpoint, reverse migration, or parity comparison ran. Source rollback and protected export were retained. | [Run 36176504323](https://github.com/centerionware/not-k8s/actions/runs/36176504323); `/tmp/nodemigrate-36176504323/k3s.log` |
| 2026-09-25 | `77e023f1349a6345f5a2bccd1e3a838887b3bbc5` | Branch-runtime upstream Kubernetes+Cilium migration | Source fixture passed; all 54 CRDs imported. CertificateRequest admission returned HTTP 500 because the cert-manager webhook was unreachable while destination Cilium/workload networking failed to initialize. Rollback restored the source and retained the protected export. No target fixture checkpoint, reverse migration, or parity comparison ran. | [Run 36176504323](https://github.com/centerionware/not-k8s/actions/runs/36176504323); `/tmp/nodemigrate-36176504323/kubernetes.log` |
| 2026-09-25 | `7292bb8880cb1096c6fce195a4bbc9c9368d2ac1` | Focused nodemigrate validation | Nodemigrate unit/integration checks, packaging, release policy, and commit convention checks passed. The newest immutable ConfigMap, multi-version user CRD, and NetworkPolicy allow/deny runtime fixtures were not included in the preceding migration run and remain unverified. | [Checks 36177264886](https://github.com/centerionware/not-k8s/actions/runs/36177264886), [tests 36177264782](https://github.com/centerionware/not-k8s/actions/runs/36177264782), [release policy 36177265014](https://github.com/centerionware/not-k8s/actions/runs/36177265014), [commit convention 36177260453](https://github.com/centerionware/not-k8s/actions/runs/36177260453) |
| 2026-09-25 | `7292bb8880cb1096c6fce195a4bbc9c9368d2ac1` | Expanded branch-runtime fixture and five-node Docker preflight | Nodemigrate and combined-runtime builds passed in both lanes, Docker isolation preflight passed, and both source fixtures reached the allowed NetworkPolicy Job. That Job completed successfully, but a contradictory log assertion failed because its `grep -q` suppresses matching output. Both lanes stopped before the deny probe and before invoking nodemigrate; no migration stage or parity was tested. Harness fix is in the current worktree; rerun pending. | [Run 36179275793](https://github.com/centerionware/not-k8s/actions/runs/36179275793); logs `/tmp/nodemigrate-36179275793/` |
| 2026-09-25 | `64baa5ab795bfd71fed1ffe193eb08e974e5345d` | Branch-runtime K3s+Cilium and upstream+Cilium migration | Both scoped utility/runtime builds and the Docker five-node isolation preflight passed. K3s source fixture passed, captured 582 objects/59 CRDs, imported all CRDs, and reached destination API readiness; `apply-sysctl-overwrites` exited 0 in CRI but Pod status stayed Running, so later Cilium init/agent, target workloads, reverse migration, and parity were not reached. Upstream source fixture passed and imported 55 CRDs, then CertificateRequest admission failed because destination cert-manager webhook was unreachable while CNI was unavailable; rollback and protected export passed. The new CRI task snapshot showed no live task for exited init containers but did not capture the event payload. | [Run 36183868918](https://github.com/centerionware/not-k8s/actions/runs/36183868918); logs `/tmp/nodemigrate-36183868918/`; Docker preflight passed |
| 2026-09-25 | `d4be86e33d9140faa9258eaff112bebed3216fa4` | Branch-runtime K3s+Cilium and upstream+Cilium migration | Scoped utility/runtime builds, Docker isolation preflight, and focused `nodelet` quick-check passed. K3s imported 59 CRDs and reached API readiness. API init statuses advanced through `apply-sysctl-overwrites`; `mount-bpf-fs` logged bpffs mounted, and its containerd task was absent, but Pod status still reported Running. Upstream imported 55 CRDs, then CertificateRequest admission failed because the webhook was unreachable while CNI was unavailable. Rollback/export retention passed in both lanes; target workloads, reverse migration, and parity were not reached. | [Migration 36186694756](https://github.com/centerionware/not-k8s/actions/runs/36186694756); [nodelet quick-check 36186694438](https://github.com/centerionware/not-k8s/actions/runs/36186694438); logs `/tmp/nodemigrate-36186694756/` |
| 2026-09-25 | `7da932bf9f15c943a8aa025739b5e6706bc95386` | Branch-runtime K3s+Cilium and upstream+Cilium migration | Nodemigrate/combined-runtime builds, five-node Docker isolation preflight, and focused `nodelet` quick-check passed. K3s init containers progressed to the Cilium agent, which then exhausted ten retries updating `CiliumNode/runnervmtr4k5` with HTTP 409 “object has been modified”; target CNI/workloads, reverse migration, and parity were not reached. Upstream imported 55 CRDs, then CertificateRequest admission failed because the cert-manager webhook was unreachable while CNI was down. Both lanes restored source and retained protected exports. Logged Cilium CRI events included Pod metadata, so the missing-metadata fallback was not exercised. | [Migration 36189354168](https://github.com/centerionware/not-k8s/actions/runs/36189354168); [nodelet quick-check 36189351742](https://github.com/centerionware/not-k8s/actions/runs/36189351742); logs `/tmp/nodemigrate-36189354168/` |

Update this table after each implementation or verification change. Record the
exact SHA, workflow run, source distribution, CNI, and whether each stage
passed, failed, or was skipped.

The integration workflow currently defaults to K3s `v1.35.0+k3s1`, Cilium
`1.20.2`, cert-manager `v1.21.2`, and Helm `v3.17.3`. The kubeadm lane follows
the Kubernetes stable minor at dispatch time. Override the pinned values with
the workflow environment when testing a different compatibility combination,
and record the resolved versions in the run log.


## Current follow-up (2026-09-29)

Run [36502166212](https://github.com/centerionware/not-k8s/actions/runs/36502166212)
at SHA `7bb853b462aaaad48ca3708e59a082c24e0b8525` used branch-built
`nodemigrate` and combined `notk8s` components with Cilium KPR and the five-node
Docker migration enabled. The upstream Kubernetes single-node round trip
passed through return. K3s also completed return and passed Node replacement
audit, then failed the returned-stage metrics-server rollout: its in-cluster
API request to `10.43.0.1:443` repeatedly returned `no route to host`; other
pod/service probes had the same symptom during recovery. Cilium later reported
KPR enabled and controllers healthy. Its service and BPF load-balancer maps
were not captured, so this remains an unresolved runtime failure. Docker
five-node topology, Cilium setup, and recovery preflight passed, but the next
step exposed unsupported container-to-container `docker cp`. The helper now
uses a host temporary-file transfer. Failure diagnostics now capture Cilium
service, BPF LB, and endpoint maps. Shell syntax, focused migration diagnostics,
and diff checks pass locally; the script fixes have not been exercised live.
Do not retry migration until the K3s routing failure has a concrete fix and
both fixes can be verified together. Exact lane jobs: Docker `109195297329`,
K3s `109195297576`, upstream `109195297739`; one-time captured logs are under
`/tmp/nodemigrate-36502166212/`. Focused
[`nodemigrate,nodecontroller` quick-check run 36501891816](https://github.com/centerionware/not-k8s/actions/runs/36501891816)
passed on the tested SHA. No regular build or full e2e gate ran.

The `4c85d3af` PR validation and crate-test workflows passed at commit
`4c85d3afaf96b10a0790043d761baa4343713c27`, including the newly extended
Cilium diagnostic command path's shell syntax. The workflow skipped runtime
migration and Docker-preflight jobs. The completed K3s artifact also contains
repeated Cilium warnings about missing endpoint links and deleted
`CiliumEndpoint` objects. Those warnings are diagnostic leads only; absent
service/BPF maps prevent establishing whether they explain the ClusterIP
failure. No migration rerun has been started.

To isolate the Cilium issue before another migration attempt, the branch adds
an optional CI restart-only probe mode. Run
[36506647392](https://github.com/centerionware/not-k8s/actions/runs/36506647392)
passed at SHA `a17fddd844a6882a075a3c461cfccd386f588e3f`: the branch combined
runtime built, the K3s+Cilium workload fixture passed, K3s restarted with its
Node identity unchanged, and all workload, storage, API-inventory, and durable
state checks passed again. Therefore, the plain K3s service restart alone does
not reproduce the prior route outage. The probe is now extended to capture
Cilium service/BPF/endpoint maps and test the same-name Node deletion and
re-registration sequence that nodemigrate uses. It still will not invoke
nodemigrate or import a migration export. This diagnostic result cannot satisfy
a migration merge gate; no migration rerun has started.

## Current follow-up (2026-09-27)

Run [36300720876](https://github.com/centerionware/not-k8s/actions/runs/36300720876) at SHA `958f75fd85c7a8145765432a6f80590f7995c9b9` failed both the K3s+Cilium and upstream Kubernetes+Cilium lanes. Docker five-node preflight and utility/runtime builds passed. K3s never reached its nodestore workload checkpoint because API discovery continued to omit `nodes.metrics.k8s.io` and `pods.metrics.k8s.io` after the 60-second inventory retry, despite APIService `Available=True`; upstream reached its checkpoint but strict source-object parity failed. Neither lane completed reverse migration. Full logs are under `/tmp/nodemigrate-36300720876/`.

The branch APIService freshness change passed focused quick-check [36301964871](https://github.com/centerionware/not-k8s/actions/runs/36301964871), but migration run `36301989587` still omitted metrics resources after APIService `Available=True`, so stale informer state was not the cause. Source review found v2 aggregated discovery returned empty resources for APIService groups. The current follow-up fetches each backend APIResourceList and converts base resources/subresources into the v2 response; check and runtime retry are pending. Both lanes also reproduced the independent CSI volume-catalog mismatch. No PV/PVC/backing-volume deletion was observed.
