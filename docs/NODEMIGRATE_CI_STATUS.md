# nodemigrate CI and integration status

Last updated: 2026-09-28

This is the living CI record for the scope in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). The user-specific testing policy
is recorded there and overrides conflicting general `AGENTS.md` gates for
this objective.

The automatic nodemigrate check
[36424188417](https://github.com/centerionware/not-k8s/actions/runs/36424188417)
failed at SHA `b140e73d7e57fdafdcb9105767b9e6056954abb6`: Rust compilation
showed `DynamicObject.types` is optional, so the CRD no-rewrite predicate's
direct `types.kind` access was invalid. The migration dispatch
[36424227253](https://github.com/centerionware/not-k8s/actions/runs/36424227253)
failed at the same compile step in Docker, K3s, and upstream; no migration
runtime ran. The predicate now reads the optional TypeMeta before checking its
kind. Both focused workflows need rerunning at the corrected SHA. No general
build or full e2e was dispatched.

Latest branch-runtime run
[36420315264](https://github.com/centerionware/not-k8s/actions/runs/36420315264)
at SHA `de890b701d61130b3e6ce9922ffc5597e4342121` built nodemigrate and the
branch combined runtime (`notk8s --features cri`) in all three lanes; the
five-node lane also built its node image. Docker passed its Cilium KPR, five-
node control-plane recovery, and CSI-on-plugin-node checks, then fixture setup
failed because `kubectl version --client` does not support `-o jsonpath`; the
probe now reads YAML with `awk`. Upstream reached returned-stage checks and
confirmed source and nodestore retained the exact Traefik CRD schema bound,
but the source API rounded it during a full-object rewrite. The importer now
preserves a destination CRD without a PUT when its spec, labels, and
annotations already match; a focused unit regression covers unchanged and
changed cases. K3s completed both migration directions, but its returned Node
later disappeared from the API and the hostpath CSI deployment did not become
Ready. Logs show kubelet node-authorization failures for re-imported Pods,
failed CSI teardown, and no Node for the local-PV affinity; the actor that
removed the Node remains unknown. Focused nodemigrate tests and a corrected
three-lane migration rerun are pending. Artifacts are saved once under
`/tmp/nodemigrate-artifacts-36420315264/`. No general build or full e2e ran.

Latest branch-runtime run
[36418026366](https://github.com/centerionware/not-k8s/actions/runs/36418026366)
completed with failure at SHA `7e8337a08a73e6027ceaab5605eacd9acebaa0de`.
All three lanes built nodemigrate and the combined branch
`notk8s --features cri`; the five-node lane also built its image. Exact job
results: Kubernetes `108913755010`, K3s `108913755239`, Docker
`108913755246`; dispatch validation `108913756721` was skipped. Cilium KPR
and five-node recovery passed, and the CSI verifier passed on the Node hosting
the singleton plugin. The Docker fixture then failed because its standalone
entrypoint did not set `NODEMIGRATE_KUBECTL_IMAGE` before creating RBAC Jobs.
Both single-node lanes migrated source state into nodestore, then failed at
the new CRD diagnostic because the target APIs do not recognize kubectl's
`crd` short name. The probe now uses the full CRD resource name; the fixture
derives its kubectl image from the active client version. No nodestore
checkpoint, return migration, or CRD precision result was reached. Artifacts
are saved under `/tmp/nodemigrate-artifacts-36418026366/`. A corrected focused
rerun is pending; no general build or full e2e ran.

Latest branch-runtime run
[36414625541](https://github.com/centerionware/not-k8s/actions/runs/36414625541)
completed with failure at SHA `b5bd5bded1e3016dbffebc6e68f3956f495c7883`.
All three jobs built `nodemigrate` and the branch combined
`notk8s --features cri`; the five-node job also built its node image. Exact
results: Docker `108902678241`, Kubernetes `108902678544`, and K3s
`108902678664`; validation `108902680012` was skipped for workflow dispatch.
Docker verified five-node Cilium KPR recovery after cp-1 loss, then the
fixture's CSI check failed because it required the singleton hostpath CSI
StatefulSet to register on all five Nodes although its only plugin Pod ran on
`worker-2`. The check now targets plugin-hosting Nodes. K3s passed source and
nodestore checkpoints and completed the return, but post-return hostpath CSI
failed its five-minute readiness wait while the Node and Cilium state changed.
Kubernetes reached the returned checkpoint, then strict parity again reported
the two Traefik CRD `priority.maximum` values changing from
`9223372036854775000` to `9223372036854776000`. Artifacts were downloaded once
under `/tmp/nodemigrate-artifacts-36414625541/`. The CSI-check correction and
K3s/CRD follow-up remain pending. No general build or full e2e was dispatched.

Latest branch-runtime run
[36411280751](https://github.com/centerionware/not-k8s/actions/runs/36411280751)
completed with failure at SHA `14bbe10d8b15dc87fd6ac91b8bdef5d13c6db648`.
Every migration lane built `nodemigrate` and the branch combined
`notk8s --features cri`, compiling all packaged components changed in this
branch; the five-node lane also built its node image. Docker preflight
`108891820491` fixed the CoreDNS loop but Cilium still could not resolve
`cp-1` before CoreDNS was available. Its API host is now set to cp-1's
reachable IP. K3s `108891821084` completed return migration and reported the
replacement Node Ready, but post-return hostpath CSI did not become ready amid
Cilium/Node readiness churn; the logs do not establish which actor caused the
Node/Pod changes. Upstream Kubernetes `108891820709` reached returned-stage
checks, then strict parity again found Traefik CRD `priority.maximum` values
change from `9223372036854775000` to `9223372036854776000`; parity remains
strict and the responsible migration hop is not yet isolated. Focused
nodemigrate crate tests passed at the same SHA in
[36411280754](https://github.com/centerionware/not-k8s/actions/runs/36411280754).
Artifacts were downloaded once to `/tmp/nodemigrate-artifacts-36411280751/`.
The Cilium address correction and latest findings are pending a focused
migration rerun. No general build or full e2e workflow was dispatched.

Latest branch-runtime run
[36407648434](https://github.com/centerionware/not-k8s/actions/runs/36407648434)
completed with failure at tested SHA `7309de99f5d653961d8e4ce2a331b655d07924ee`.
All three migration jobs built `nodemigrate` and the branch combined
`notk8s --features cri`; the five-node job also built its node image. Exact job
results: Docker preflight `108880115025`, K3s `108880115167`, and Kubernetes
`108880115214`. The Docker fixture confirmed CoreDNS's loop plugin was
forwarding to `127.0.0.1`, explaining the failed source NetworkPolicy probe.
K3s returned the node Ready, then the CSINode owner-reference PATCH returned
404 after its read; a CSI registrar replacement race is the leading
explanation, but the log does not identify the actor. Kubernetes completed workload and
storage checks but strict parity caught Traefik CRD schema bounds changing
from `9223372036854775000` to `9223372036854776000`; this remains an actual
data difference and parity stays strict. Full artifacts are saved once under
`/tmp/nodemigrate-artifacts-36407648434/`; do not download the same logs again.
The confirmed fixture and CSINode fixes are being checked in the next focused
migration run. The Traefik CRD precision defect remains unresolved.

The preceding branch-runtime run
[36402381606](https://github.com/centerionware/not-k8s/actions/runs/36402381606)
at SHA `8ce9b2691f0c639f10017f55edfa23fd094ad500` also built the combined
runtime and failed in the same runtime areas. Its artifacts are under
`/tmp/nodemigrate-artifacts-36402381606/`; its job logs are under
`/tmp/nodemigrate-{k3s,kubernetes,docker}-36402381606.log`.
The corrected focused nodeapiserver quick-check passed at SHA `4c718dc0` in
[36403498362](https://github.com/centerionware/not-k8s/actions/runs/36403498362)
and includes the CRD protobuf-double round-trip regression. The earlier
quick-check at SHA `8ce9b269` failed because its assertion required an
integer-shaped JSON spelling from a protobuf `double`; it now compares the
numeric `f64` value. The previous completed branch-runtime failure at SHA
`fa8c6ec97c3a3afecd1162304bcecc0694f19a1d` was run at
[36397547121](https://github.com/centerionware/not-k8s/actions/runs/36397547121).
That run's focused nodemigrate crate tests passed at
[36397531245](https://github.com/centerionware/not-k8s/actions/runs/36397531245);
branch workflow validation passed at
[36397531246](https://github.com/centerionware/not-k8s/actions/runs/36397531246);
commit convention passed at
[36397528336](https://github.com/centerionware/not-k8s/actions/runs/36397528336).
Kubernetes reached `PASS stage=returned`, then strict parity confirmed two
Traefik CRD `priority.maximum` changes from `9223372036854775000` to
`9223372036854776000`; this is an actual `float64` value difference and strict
parity remains in force. K3s imported state and reported the replacement Node
Ready, but returned-stage
verification later observed the Node missing; the captured logs do not
establish what removed it. The CSI wait and separate static/CSI fixture Pods
are in the tested SHA. The five-node source workload failed its NetworkPolicy
allow probe with CoreDNS in CrashLoopBackOff; captured diagnostics lack
CoreDNS container output, so its cause is unknown. Follow-up diagnostics now
collect current and previous CoreDNS logs and capture Node/Lease state plus
K3s/kubelet journals on readiness failure. The earlier quick-check log is at
`/tmp/nodemigrate-quickcheck-36402378386.log`. Complete migration logs for
run `36397547121` are saved at `/tmp/nodemigrate-36397547121.log`. No regular
build or full e2e was dispatched.

Previous validation attempt at SHA `f8cc4777d8356e70a91752f959839b1e3f549583`
failed before running tests: `nodemigrate` and each migration lane hit the same
compile error in `wait_for_discovery`, which wrapped an `anyhow::Error` with
`Error::new`. The error was corrected by adding context directly. Utility test log:
`/tmp/nodemigrate-tests-36389588658.log`; migration build log:
`/tmp/nodemigrate-kubernetes-36389615059.log`. Runs:
[nodemigrate checks 36389588658](https://github.com/centerionware/not-k8s/actions/runs/36389588658)
and [branch migration 36389615059](https://github.com/centerionware/not-k8s/actions/runs/36389615059).
The migration jobs did not reach their runtime lane or five-node fixture.

Latest branch-runtime run
[36385002094](https://github.com/centerionware/not-k8s/actions/runs/36385002094)
at SHA `9c7897f0d917e51b9e3d8e81ee6a843e3332c8f2` failed after the runtime
builds completed. All three migration jobs built `nodemigrate` and branch
`notk8s --features cri`; compiling the combined runtime included the packaged
component crates from this branch. The dedicated `nodemigrate` quick-check
passed at the same SHA in
[36384989298](https://github.com/centerionware/not-k8s/actions/runs/36384989298).
The Docker preflight created source PVs but its static hostPath fixture could
schedule on a different node than the directory owner; the fixture now pins
the PV to `cp-1`. K3s passed direct retained-API readiness but timed out
repeatedly on per-group discovery while Cilium was unhealthy; discovery now
tries aggregated discovery before its compatibility fallback. Upstream passed
return-stage workload/storage checks, including PVC data and all 119 listable
API resources, but strict parity found Traefik CRD content changes plus
destination-generated workload revisions and `nodebootstrap:` RBAC. Those
differences still need diagnosis; this run did not pass either full round-trip
gate. Logs are saved under `/tmp/nodemigrate-{docker,k3s,kubernetes}-
36385002094.log`; downloaded artifacts are in
`/tmp/nodemigrate-artifacts-36385002094/`. No general build or full e2e
workflow was dispatched.

Previous branch-runtime run
[36381052266](https://github.com/centerionware/not-k8s/actions/runs/36381052266)
at SHA `74352dbd90ffa3df71ff5adb208adf93c9dad1f7` is terminal failure. The
targeted `nodemigrate` quick-check passed at
[36381051939](https://github.com/centerionware/not-k8s/actions/runs/36381051939).
All three migration jobs built `nodemigrate` and branch `notk8s --features cri`,
including the packaged runtime components. The upstream lane completed the
return import and Node registration; the source-running standalone Pod was
recreated and Ready, but Traefik ingress and Gateway requests returned 504.
The reverse export contained 11 EndpointSlices managed by this project's
`nodecontroller`; the importer treated that manager label as user-owned and
restored stale endpoints. nodemigrate now skips that controller-managed label
so the destination can recalculate it. K3s failed its nodestore checkpoint
when Cilium sandbox churn left a stale Ready Pod whose CRI container had
exited; the fixture now requires an API-reported running container and retries
that specific transient exec failure while it refreshes Pod status. The
five-node kubeadm/Cilium isolation and cp-1 recovery passed after the shared
mount fix, but fixture installation stopped because the node image lacks
`git`; `git` and `cri-tools` are now included. A targeted nodemigrate
quick-check and a new branch-runtime migration run are pending for these fixes.
No general build or full e2e workflow was dispatched. Saved logs:
`/tmp/nodemigrate-docker-36381052266.log`,
`/tmp/nodemigrate-k3s-36381052266.log`, and
`/tmp/nodemigrate-kubernetes-36381052266.log`.

Latest branch-runtime run
[36377385003](https://github.com/centerionware/not-k8s/actions/runs/36377385003)
at SHA `d2a7e06abeed8ce349fd8c53795809486004c876` is terminal failure. Each
lane built `nodemigrate` and branch `notk8s --features cri`, including the
packaged runtime component crates. K3s completed forward migration and
119-resource parity; return rolled back after the fixture selected a Running
Pod with an exited container. The fixture now selects only a non-terminating,
fully Ready Pod and prints candidates if none qualify. Upstream completed both
migrations and return readiness after a transient cert-manager webhook HTTP
500 was retried, but its standalone source-running Pod had become Failed
(exit 137) and was not restarted. The importer now records that source state
in the protected export and recreates a terminal same-name destination Pod;
focused CI validation is pending. The five-node Docker preflight built both
binaries and recovered Kubernetes API/Node readiness, but Cilium could not
restart because `/sys/fs/bpf` lost shared mount propagation. The preflight now
restores `/sys`, bpffs, and `/run` propagation before enabling kubelet; runtime
verification is pending. No general build or full e2e workflow was dispatched.

Latest branch-runtime run
[36373995554](https://github.com/centerionware/not-k8s/actions/runs/36373995554)
at SHA `539ea445a7d7bc1c73e94ffe5c759eb5e9232751` compiled both `nodemigrate`
and the branch `notk8s --features cri` combined runtime in all three jobs; this
compiles the packaged runtime component crates. K3s reached nodestore but
failed its workload checkpoint while Cilium reinitialized and Pod/Service
routing was unavailable; the fixture tried to exec into an exited container
before checking CNI/CoreDNS readiness. Upstream passed forward migration and
the nodestore checkpoint, then return import failed repairing a CertificateRequest
while the cert-manager webhook was unavailable; reference repair did not retry
transient API errors. The
five-node preflight installed kubeadm/Cilium and recovered after control-plane
loss, but its script restarted cp-1 twice and the second recovery check failed
without identifying the failed predicate. The harness now uses one recovery
cycle, reports each readiness result and collects diagnostics; fixture stages
explicitly wait for Cilium and CoreDNS before workload checks; nodemigrate
retries transient API failures during reference repair. These changes need a
new migration run. No general build or general e2e workflow was dispatched.

At fix SHA `dfa3325594be1b150f07576f0ecc2060f7554deb`, focused nodemigrate
crate tests and packaging checks passed in
[36371692594](https://github.com/centerionware/not-k8s/actions/runs/36371692594).
Branch-runtime migration run
[36371692313](https://github.com/centerionware/not-k8s/actions/runs/36371692313)
is terminal failure. Both single-node jobs and the five-node job compiled
`nodemigrate`; all three compiled the branch `notk8s` combined binary with CRI,
which compiles the packaged runtime component crates, including branch fixes.
The upstream lane returned a Ready replacement Node and completed API import,
then failed the PDB eviction assertion. The K3s lane still times out in full
destination discovery. Five-node Docker isolation and control-plane recovery
passed, but the migration fixture failed because the restarted `cp-1` API was
not ready when its fixture setup began. No general build or general e2e
workflow was dispatched. Follow-up harness fixes are pending migration CI.

Release-backed migration run
[36351022258](https://github.com/centerionware/not-k8s/actions/runs/36351022258)
at branch SHA `55978b86`, with `runtime_source=release` and
`cilium_kpr=true`, is terminal failure against regular `v0.8.0`. Docker
preflight passed. The K3s lane's import rejected three Gateway API CRDs on CEL
type and estimated-cost validation; two dependent Gateway objects remained
unavailable after 59 five-second retries. The upstream lane also failed during
import. Neither lane reached return migration, so this run does not validate
retained-API readiness or either round trip. It did not enable general build or
e2e workflows.

Latest branch-runtime run
[36369153385](https://github.com/centerionware/not-k8s/actions/runs/36369153385)
at SHA `1d37976d` used `runtime_source=branch` and `cilium_kpr=true`. The
nodemigrate and branch `notk8s` release builds passed; the latter compiles all
seven default runtime components. Docker five-node preflight passed. The K3s
lane completed forward migration and return API readiness, then all 20 full
discovery probes timed out and the utility restored nodestore. The upstream
lane completed forward migration, returned API readiness after one refused
connection, and applied all 55 CRDs on return. The returned node was Ready in
early snapshots, but later snapshots had no Node and all workloads became
unschedulable; rollback restored the source. The fixture explicitly enables
same-name replacement, and the return path deleted the Node after starting the
retained service without restarting kubelet. A focused fix now restarts the
retained service after deleting a replacement Node; migration validation is
pending.
Both runs skipped 18–21 source `CiliumIdentity` records as destination-rebuilt
runtime state, confirming the exporter change is active. This did not resolve
either round trip: K3s discovery still stalls, while upstream return Cilium
readiness still fails. The upstream log also contains identity allocator
collision warnings; their relationship to the readiness failure is unproven.
Focused nodemigrate checks passed in
[36369153229](https://github.com/centerionware/not-k8s/actions/runs/36369153229),
and quick-check for the other modified runtime crates passed in
[36369712853](https://github.com/centerionware/not-k8s/actions/runs/36369712853).
Artifacts are in `/tmp/nodemigrate-36369153385/`.

Previous branch-runtime run
[36366505575](https://github.com/centerionware/not-k8s/actions/runs/36366505575)
at SHA `50b2805f` passed nodemigrate and branch-runtime builds; Docker
kubeadm/Cilium preflight passed. K3s passed direct core Namespace readiness,
then all 20 full destination-discovery probes timed out within the bounded
five-minute retry. At the `01:54:12Z` target-state snapshot, Cilium agent,
Envoy, and cert-manager Pods reported Ready, but probes to the cert-manager
webhook ClusterIP and its ready endpoint failed. A later `01:59:17Z` snapshot
showed Cilium agent/Envoy and cert-manager Pods unhealthy after recovery or
rollback activity; that later state does not establish the cause of the
discovery timeouts. During the preceding nodestore stage, Cilium failed to
recreate a metrics-server identity because the imported `CiliumIdentity` with
that ID already existed. The exporter had selected 27 CiliumIdentity objects
for migration even though the fixture's parity comparison treats these as
destination-rebuilt runtime state. The exporter now follows that contract;
runtime validation is pending. The upstream lane passed API readiness after one
connection-refused attempt, but the cert-manager webhook ClusterIP timed out
while importing its CertificateRequest. Both migration lanes failed and rolled
back. Focused nodemigrate quick-check passed at code SHA `801b3275` in
[36366260390](https://github.com/centerionware/not-k8s/actions/runs/36366260390).
Artifacts are in `/tmp/nodemigrate-36366505575/`.

Previous branch-runtime run
[36363999391](https://github.com/centerionware/not-k8s/actions/runs/36363999391)
at SHA `27b4152b` passed nodemigrate and branch-runtime builds; Docker
kubeadm/Cilium preflight passed. The K3s return passed retained API readiness
using the direct core Namespace probe, then full destination discovery returned
HTTP 503 before namespace import; rollback restored nodestore. The upstream
lane's readiness probe recovered after one connection-refused attempt and all
55 CRD apply requests passed. Its CertificateRequest import failed because the
cert-manager webhook ClusterIP timed out. Both lanes failed and rolled back.
The importer now retries initial API discovery for up to five minutes, with a
ten-second deadline per probe. Its focused nodemigrate quick-check initially
failed at SHA `78d2869b` because the retry-delay constant was missing; that was
corrected at SHA `801b3275`, where quick-check
[36366260390](https://github.com/centerionware/not-k8s/actions/runs/36366260390)
passed. Live validation is pending. Artifacts are in
`/tmp/nodemigrate-36363999391/`.

Previous branch-runtime run
[36361932369](https://github.com/centerionware/not-k8s/actions/runs/36361932369)
at SHA `890e8d65` passed nodemigrate and branch-runtime builds. K3s source,
forward migration, and the first nodestore fixture passed. Its return leg
started K3s, but all 20 retained-API probes exceeded the ten-second timeout
while `ready()` performed full API discovery; the five-minute retry then
rolled back to nodestore. Post-rollback fixture exec failed with
`CONTAINER_EXITED`. The new implementation directly lists core Namespaces to
avoid discovery fan-out; focused nodemigrate quick-check
[36363774087](https://github.com/centerionware/not-k8s/actions/runs/36363774087)
passed at SHA `2884eeaa`. The upstream lane failed
importing `CertificateRequest migration-test-1` with HTTP 500. Five-node
Docker preflight failed at `Probe kubeadm nodes`. The nodelet quick-check
[36361932310](https://github.com/centerionware/not-k8s/actions/runs/36361932310)
passed at the same SHA. Full e2e and general build workflows were not run.
Artifacts are in `/tmp/nodemigrate-36361932369/`.

Run [36343008296](https://github.com/centerionware/not-k8s/actions/runs/36343008296)
at SHA `7f1c53f1` is terminal `cancelled`. Its K3s lane completed forward
migration and API export, then started retained K3s for the return leg and
stalled inside retained-API readiness for over an hour. The final log artifact
is `/tmp/nodemigrate-36343008296-k3s/nodemigrate-k3s.log`; it ends immediately
after `waiting for retained destination API readiness`, so the underlying API
failure is not known. A timeout and per-probe error logging were added;
focused `nodemigrate` and `nodebootstrap` tests passed at fix SHA `2d98e05e`
in [run 36350477684](https://github.com/centerionware/not-k8s/actions/runs/36350477684);
the PR's targeted `nodemigrate` test also passed at SHA `c9007b9c` in
[run 36350708110](https://github.com/centerionware/not-k8s/actions/runs/36350708110).
The bounded probe implementation passed focused CI, but a live post-fix
branch-runtime return migration remains unverified. The paired upstream lane
failed earlier and is not evidence for the current branch.

The [bug and fix tracker](NODEMIGRATE_BUGS.md) lists confirmed defects by
owning component, branch fix, and focused test evidence. The intended
`v0.8.1` regular runtime and standalone utility are based on this branch. Branch
runtime tests are the primary migration target; `v0.8.0` can remain an optional
published-runtime regression baseline, but is not required or exclusive.

Branch-runtime run
[36359977515](https://github.com/centerionware/not-k8s/actions/runs/36359977515)
at SHA `f538a8d3` passed all focused `nodemigrate`, `notk8s`, and branch runtime
builds. Both K3s and upstream lanes passed source and first `stage=nodestore`
checks, including the non-root `emptyDir` test after the selector correction.
K3s return timed out all 20 retained-API probes; upstream return began with an
API connection refusal and failed import when the cert-manager validation
webhook ClusterIP timed out. Both migration utilities restored nodestore, but
the post-rollback workload exec checkpoint failed on stale/missing CRI state.
The five-node job passed kubeadm/Cilium readiness and cp-1 loss recovery, then
failed because it copied the hostpath script to `/var/tmp` while the
integration script requested `/tmp`. The coordinator now passes its configured
helper path through. App container reconciliation now uses nodelet's
running/newest CRI selection; focused quick-check and a live rerun are pending.
Artifacts are in `/tmp/nodemigrate-36359977515/`.

Run [36357521280](https://github.com/centerionware/not-k8s/actions/runs/36357521280)
at branch SHA `22848696` built `nodemigrate` and the branch combined runtime
successfully. The Docker preflight passed five-node kubeadm/Cilium setup and
control-plane-loss recovery, then failed copying the hostpath helper because
the source and destination collided at `/tmp`; the copy now uses `/var/tmp`.
Both single-node migration lanes reached the nodestore fixture and failed its
non-root `emptyDir` exec check. Code inspection points to CRI lookups choosing
stale sandboxes or container attempts. Nodelet now selects by current Pod UID
when available and prefers ready sandboxes and running container attempts;
focused regressions are added. Quick-check
[36359505569](https://github.com/centerionware/not-k8s/actions/runs/36359505569)
at SHA `846a5a94` failed because the selector was private to a sibling test
module and its closure borrowed a sandbox while moving it. Commit `8f7df53e`
corrects both issues, and focused `nodelet` quick-check
[36359755288](https://github.com/centerionware/not-k8s/actions/runs/36359755288)
passed at that SHA. Live migration verification remains pending. Artifacts are in
`/tmp/nodemigrate-36357521280/`.

Migration run [36359505599](https://github.com/centerionware/not-k8s/actions/runs/36359505599)
at SHA `846a5a94` failed before migration execution: the K3s/upstream branch
runtime builds and five-node combined-runtime build hit those compile errors.
The corrected code needs a new branch-runtime migration run.

Branch-runtime migration run
[36352851628](https://github.com/centerionware/not-k8s/actions/runs/36352851628)
ran at SHA `655ba83c` with `runtime_source=branch`, `cilium_kpr=true`, and
`five_node_migration=true`. The `nodemigrate` and `notk8s` component builds
passed, and all five isolated Kubeadm/Cilium nodes became Ready. The K3s lane
passed forward migration and its nodestore checkpoint (700 API objects, 59
CRDs), then failed returning to K3s: API readiness timed out, Cilium's health
listener reported `address already in use`, and workload Service/Pod IPs were
unreachable. KPR was enabled and both `kube-proxy` and `nodeproxy` were
inactive, so this was not a competing Service-proxy configuration. The branch
now stops nodestore CRI pod sandboxes and exact-identity leftover Cilium
processes before starting the retained cluster; runtime verification is
pending.

The upstream lane passed forward migration, its nodestore checkpoint (667 API
objects, 55 CRDs), and retained API readiness, then failed during return import
because `migration-apps/migration-legacy-token` was missing. The branch now
recreates the missing Secret with destination-bound token credentials while
preserving its other data; runtime verification is pending.

Run [36355485146](https://github.com/centerionware/not-k8s/actions/runs/36355485146)
at SHA `26f81b53` passed all component builds and the five-node Kubeadm/Cilium
preflight, but the full run ended in failure for all three migration jobs. The
K3s return stopped nodestore CRI sandboxes and confirmed there were no leftover
source Cilium processes. This removed the earlier duplicate Cilium health
listener bind error, but retained K3s still failed Service-IP traffic and API
readiness; its return export included 30 generated CiliumEndpoints and one
CiliumNode. The branch now omits those generated Cilium runtime records while
preserving Cilium policies, then lets the target agent reconcile them. Runtime
confirmation remains pending.

The upstream lane recreated the missing legacy ServiceAccount token Secret
with destination credentials and completed API import. The returned Node then
remained NotReady with Cilium/Pod runtime readiness failures; the cause is not
confirmed. The five-node preflight reached five Ready nodes and recovered the
cluster after control-plane loss, but its full migration fixture could not find
the hostpath setup script in `cp-1` after the preflight had restarted that
container. The script is now copied again after restart and before the fixture.
Run logs are saved under `/tmp/nodemigrate-36355485146/`.

Run 36352851628's Docker wrapper failure was fixed: run 36355485146 confirms
the probe reached the five-node migration fixture. The worker admin kubeconfig
fix at `1bcd76e3` remains untested by a successful five-node migration.

The five-node return coordinator fix at SHA `1bcd76e3` copies kubeadm's
recovered `/etc/kubernetes/admin.conf` to both workers before their return
migrations, because `kubeadm join` does not provision that admin credential on
workers. PR shell validation passed in
[36353488792](https://github.com/centerionware/not-k8s/actions/runs/36353488792).
Run 36352851628 predates this fix and cannot validate the worker return path;
runtime validation at `1bcd76e3` remains pending.

At SHA `42162ef20afca2e4b4f616cc1d83fbd05071a9ae`, focused `nodelet`
quick-check [36311005893](https://github.com/centerionware/not-k8s/actions/runs/36311005893)
passed. Migration run [36311005956](https://github.com/centerionware/not-k8s/actions/runs/36311005956)
passed its five-node Docker preflight and both migration lanes passed the
source and nodestore PV/PVC binding and StatefulSet payload checks. Both then
failed strict source-object parity at nodestore; neither ran the return
migration. The successful storage checks confirm that migration did not delete
the PVs, PVCs, or payloads in either lane. Earlier branch-runtime run
[36310603619](https://github.com/centerionware/not-k8s/actions/runs/36310603619)
at SHA `1dc1c2db` failed both nodestore checkpoints because the registered
hostPath driver had no CSINode owner reference to the current Node. That SHA
predates the Nodelet owner-reference repair. Neither lane completed its
return migration.
Latest-release run [36310054614](https://github.com/centerionware/not-k8s/actions/runs/36310054614)
fetched regular release `v0.8.0` and is now terminal (`failure`). The K3s lane
failed importing `gateways.gateway.networking.k8s.io` because Gateway metadata
CEL rule-cost estimation overflowed; its source storage checks passed and
rollback retained the protected export. The upstream lane also ended in
failure after its source fixture. Its import summary records Gateway API CRD
validation failures, HTTP 500 responses for a CertificateRequest and CSR, and
Gateway objects unavailable because their CRDs were not installed. The log
also records CoreDNS sandbox-name reservation collisions and an unreachable
cert-manager webhook during rollback. Neither lane completed a nodestore
semantic checkpoint or return migration. Source PV/PVC/data assertions passed,
and no PV deletion was reported. The branch runtime at `42162ef2` accepted the
Gateway API CRDs and reached nodestore. Upstream artifact:
`/tmp/nodemigrate-36310054614/upstream/nodemigrate-kubernetes.log`; K3s
artifact: `/tmp/nodemigrate-36310054614/artifacts/nodemigrate-k3s.log`.

At SHA `f9356dad16a549fdaf07c2a79e135a8cbfc836e9`, nodemigrate quick-check
[36313588814](https://github.com/centerionware/not-k8s/actions/runs/36313588814)
passed in 1m28s. Branch-runtime migration run
[36313589064](https://github.com/centerionware/not-k8s/actions/runs/36313589064)
completed with both migration jobs failing after the five-node preflight and
both utility/runtime builds passed. The latest-runtime-fetch step was skipped
because branch runtime binaries were built; this run did not test the separate
v0.8.0 release baseline. Both lanes passed `stage=source` and `stage=nodestore`,
including PV/PVC bindings, StatefulSet data, and the 119-resource source API
inventory. No PV deletion was reported. Strict object comparison then failed:
the migration fixture reinstalled its hostPath CSI driver for the nodelet data
root (changing its volume paths and adding the explicitly checked source-stage
mount), and Cilium regenerated `CiliumNode.spec.health.ipv4`. Neither lane
reached return migration. Job logs are saved at
`/tmp/nodemigrate-36313589064-k3s.log` and
`/tmp/nodemigrate-36313589064-kubernetes.log`.

This SHA changes existing-object imports from SSA to resource-version-checked
replacement and repairs Node-owned object references after replacement Node
registration. Its focused Rust quick-check passed; live round-trip acceptance
did not. At SHA `17281efb42343ae2f934f681f57f031a59bc6016`, targeted nodemigrate
quick-check [36315451874](https://github.com/centerionware/not-k8s/actions/runs/36315451874)
passed, including crate unit tests. Migration run
[36315595317](https://github.com/centerionware/not-k8s/actions/runs/36315595317)
failed both lanes during initial fixture setup before nodemigrate ran. The
fixture failure was fixed in SHA `b69ef4e7` by submitting the required
ServiceAccount annotations in the Secret's initial create request. The
corrected fixture's source and nodestore token/RBAC probes passed in run
[36316498148](https://github.com/centerionware/not-k8s/actions/runs/36316498148).

At SHA `b69ef4e73c0587b98f715aafe49321c27d710996`, migration run
[36316498148](https://github.com/centerionware/not-k8s/actions/runs/36316498148)
passed Docker preflight, utility/runtime builds, and both `stage=source` and
`stage=nodestore` checkpoints in the K3s and upstream lanes. PV/PVC identities,
bindings, and StatefulSet data passed at both stages. The legacy ServiceAccount
token's allowed and denied RBAC checks passed at source and nodestore. All 119
source-discovered listable APIs remained exposed at nodestore. Both lanes then
failed strict object parity on `default/csi-hostpathplugin` volume index 5
(volume name and hostPath path/type); neither reached the return migration.
The exact field values are not in the current artifact, so this is tracked as
an unresolved CSI object mismatch, not normalized away. No PV or PVC deletion
was reported. Logs are saved at
`/tmp/nodemigrate-36316498148-k3s-artifact/nodemigrate-k3s.log` and
`/tmp/nodemigrate-36316498148-kubernetes-artifact/nodemigrate-kubernetes.log`.
The current worktree adds a restricted diagnostic for the fixture CSI volume
names and hostPath values; the next migration run must supply the missing
before/after evidence.

## Most recent evidence

At code SHA `f5bcadbbff5029c6128544a771ca2bd1ba1114ca`, the strengthened
node-private export test also verifies copied node UID, labels, annotations,
and control-plane role. Focused crate tests passed in
[36349107771](https://github.com/centerionware/not-k8s/actions/runs/36349107771).
Migration-workflow validation passed in
[36349107726](https://github.com/centerionware/not-k8s/actions/runs/36349107726);
the Docker and migration jobs were skipped for the pull-request event. The
five-node runtime gate remains pending.

At code SHA `59b38a9604e20cae478d35ce737ef4c189960bf2`, the node-private
protected-export copy fix passed the focused `nodemigrate` crate tests in
[36348554916](https://github.com/centerionware/not-k8s/actions/runs/36348554916).
Migration-workflow validation passed in
[36348554980](https://github.com/centerionware/not-k8s/actions/runs/36348554980),
including shell, snapshot, Helm, API inventory, and diagnostics checks. Its
Docker preflight and migration jobs were skipped because this was a pull
request event. The fix has unit/static evidence only; no multi-node migration
has run with it.

Branch-runtime migration run
[36335580680](https://github.com/centerionware/not-k8s/actions/runs/36335580680)
was dispatched at SHA `f186930d7333a0b1c96967f2418b9be18b1155d6` with both
source lanes enabled. The five-node Docker preflight and both utility/runtime
builds passed. The upstream job finished with failure in 42m27s after source
and nodestore checkpoints passed. Source and nodestore StatefulSet PVC/PV/data
assertions passed; no PV deletion was reported. Forward migration completed.
The return import failed on three cert-manager resources because the retained
source API could not reach `cert-manager-webhook` at `10.106.197.148:443`; the
utility restored nodestore and retained its protected export. Rollback-stage
fixture verification then failed its non-root `emptyDir` marker check after a
listed Pod disappeared before `kubectl exec`; investigate this separately from
the webhook failure. The upstream target log shows a kube-proxy Pod and
`nodeproxy` both active with Cilium KPR disabled, confirming the competing
Service-proxy bug now fixed in `7bad3b91`; that fix has not yet had a migration
runtime run. Cilium agent/Envoy were unready and CoreDNS/cert-manager containers
terminated with exit 137, but their startup failure cause is unresolved. The
upstream log contains exactly 226 `jq` parse errors from the target-state
watcher merging `kubectl` stderr into `jq`; this SHA predates the fix at
`deaed970`. Log:
`/tmp/nodemigrate-36335580680-artifacts/nodemigrate-kubernetes-36335580680/nodemigrate-kubernetes.log`.
The K3s lane was cancelled at the workflow timeout; the workflow is terminal
failure. The post-fix full migration rerun remains pending.

At PR head `7bad3b9145dd5a6b0612d8eeeca2ef0fd06bbdcf`, nodemigrate now
selects the upstream kube-proxy DaemonSet or Cilium KPR as the Service router
and disables nodeproxy when either is active. It rejects simultaneous Cilium
KPR and kube-proxy before cutover. Targeted crate tests passed in
[36347140648](https://github.com/centerionware/not-k8s/actions/runs/36347140648);
migration workflow validation passed in
[36347140725](https://github.com/centerionware/not-k8s/actions/runs/36347140725).
These do not prove runtime proxy behavior. Workflow KPR is now retained through
`sudo`; runtime validation with KPR on and off remains pending.

At SHA `bb32d6c86dfe614b87fa66edf26d313b3b1fa5ad`, the target-state watcher
adds kube-proxy DaemonSet and Pod readiness to its Cilium and webhook
diagnostics, addressing a gap in the upstream failure capture above. Migration
workflow validation passed in
[36339547744](https://github.com/centerionware/not-k8s/actions/runs/36339547744),
and the targeted nodemigrate crate checks passed in
[36339547796](https://github.com/centerionware/not-k8s/actions/runs/36339547796).
These checks do not exercise the watcher against a live target. Run
36335580680 is terminal and predates this instrumentation; its artifacts were
retrieved once for review. The post-fix migration run remains pending.

Cilium kube-proxy replacement run
[36343008296](https://github.com/centerionware/not-k8s/actions/runs/36343008296)
was dispatched at `7f1c53f1531383e6ef3b9b73fe36f1fe3301216c` with
`cilium_kpr=true` and `runtime_source=branch`. As of the latest GitHub Actions
query on 2026-09-27, its upstream `Run migration` step failed after 43m18s and
the K3s `Run migration` step is still active. The terminal upstream artifact
has not been retrieved because run-level artifact review is deferred until
both lanes are terminal; the failure cause remains unestablished. Do not start
another long migration run while the K3s lane is active.
The preflight script at this SHA always installed kube-proxy, so its five-node
setup used KPR disabled despite the workflow input. The single-node source
step also lost the input across its explicit `sudo --preserve-env` allowlist,
so this run did not exercise KPR=true at either source. The allowlist is fixed
at `7bad3b91`; the run also predates initial-control-plane `--proxy=none` and
the nodeproxy-inactive assertion. Do not claim KPR-enabled source/target proxy
selection or post-fix behavior from this run. The current preflight honors the
KPR input. Five-node preflight run
[36345278305](https://github.com/centerionware/not-k8s/actions/runs/36345278305)
passed at `74068255` with `docker_only=true`, `cilium_kpr=true`, and
`runtime_source=branch`. Its uploaded log confirms five isolated kubeadm nodes,
Cilium KPR enabled with kube-proxy absent, and cluster recovery after
control-plane loss. This run did not invoke nodemigrate and does not verify the
full five-node migration gate. Artifact:
`/tmp/nodemigrate-36345278305-artifacts/nodemigrate-docker-preflight-36345278305/nodemigrate-docker-preflight.log`.

Release-backed migration run
[36332106663](https://github.com/centerionware/not-k8s/actions/runs/36332106663)
was dispatched at PR #591 head `012f1b7351da10cd78bec309e1efc6a2e9de439e`
with `runtime_source=release` and both source lanes enabled. The five-node
Docker preflight passed, and both lanes fetched the latest regular runtime
before migration. The run is terminal failure. In both lanes, the source
fixture passed its initial stage and the protected API export was written. The
release runtime rejected Gateway API CRDs because CEL rule-cost estimation
saturated at `u64::MAX` for bounded map and hostname expressions, with
additional CEL type errors for object/map and TLS fields. Gateway CRDs could
not be established, so Gateway objects became unavailable downstream. The
upstream lane also saw HTTP 500 responses importing a CertificateRequest and a
CSR. Both lanes restored their source service, stopped partial nodestore
services, and retained the protected export; neither reached nodestore
semantic checks or return migration. Source fixture PV/PVC/data checks passed
in both lanes, and no PV deletion was reported. This is a release-baseline
failure already tracked under the `nodeapiserver` CEL compatibility bugs, not
evidence that PVs are part of migration cleanup. K3s artifact:
`/tmp/nodemigrate-36332106663/nodemigrate-k3s-36332106663/nodemigrate-k3s.log`;
upstream artifact:
`/tmp/nodemigrate-36332106663/nodemigrate-kubernetes-36332106663/nodemigrate-kubernetes.log`.
The run contains the redacted Cilium CRI diagnostics, including the endpoint
selection fix for K3s source containerd. It is independent of
branch-runtime run
[36321804254](https://github.com/centerionware/not-k8s/actions/runs/36321804254):
that run's upstream lane failed at 13:37 UTC before the nodestore checkpoint.
The K3s lane later reached nodestore and started the return migration, but its
job was cancelled at the 180-minute timeout at 16:16 UTC.

At SHA `5ce3f3d613ec6caa08623e542ad79984c42c9bbc`, focused `nodemigrate`
crate checks passed in [36334107161](https://github.com/centerionware/not-k8s/actions/runs/36334107161).
Migration-workflow static validation passed in
[36334107153](https://github.com/centerionware/not-k8s/actions/runs/36334107153);
its Docker and migration jobs were skipped for the pull-request event. This
commit adds return-migration phase progress messages; the current v0.8.0
runtime run began before those messages and cannot verify them.

Branch-runtime migration run
[36321804254](https://github.com/centerionware/not-k8s/actions/runs/36321804254)
tests SHA `4c319d08` with source CNI Cilium. Its Docker five-node topology
preflight passed. The upstream Kubernetes source fixture passed its storage
checkpoint: the four PVCs and PVs (including the StatefulSet payload) were
bound before migration. Forward export captured 574 API objects and the target
accepted all 55 CRDs. The target Cilium agent repeatedly failed writes to
`/proc/sys/net/ipv6/conf/cilium_host/forwarding` with `EROFS`; Cilium did not
become ready, and cert-manager admission requests failed. The lane restored the
source and retained its protected export without reaching nodestore. The K3s
lane passed source and nodestore behavior, storage, and API parity checkpoints;
then `nodestore -> k3s` started and wrote a second protected export, but its
job was cancelled at the 180-minute timeout before returned-source checks
completed. No PV deletion was reported. Neither lane completed the round trip,
and these observations do not yet identify a root-caused component defect.
This run predates SHA `ddc2cfdf`, which adds redacted Cilium CRI
security-context and OCI mount diagnostics. Its completed upstream artifact is
`/tmp/nodemigrate-36321804254-upstream/nodemigrate-kubernetes.log`; K3s
artifact: `/tmp/nodemigrate-36321804254-k3s/nodemigrate-k3s.log`. SHA
`c50a812d` makes the probe try both nodelet and K3s containerd endpoints and
select the CRI record whose Pod UID matches the API Cilium agent; mocked
endpoint-selection and no-environment-leak checks passed. The current-head
release-backed run is exercising these diagnostics.

Latest branch-runtime migration run
[36320456287](https://github.com/centerionware/not-k8s/actions/runs/36320456287)
tested SHA `ad861cdc7f57876c1c865a7d0b42a2b52bd3b8d4`. The five-node Docker
preflight passed its kubeadm 3-control-plane/2-worker Cilium setup and
control-plane recovery probe. Both single-node lanes passed source and
nodestore behavior checks, PV/PVC binding and payload checks, strict API
object parity, and the new assertion that the source CSI `/dev` volume remains
present immediately after forward migration and after fixture setup. They
then failed only the application snapshot comparison because the target
StatefulSet had API-defaulted `minReadySeconds: 0`, absent at source. Neither
lane reached return migration. Artifacts are at
`/tmp/nodemigrate-36320456287-artifacts/`. The snapshot filter now treats only
that default as equivalent, with a focused test that nonzero values remain
significant; the dedicated migration rerun is pending.

Latest migration run
[36318123614](https://github.com/centerionware/not-k8s/actions/runs/36318123614)
tested code SHA `a95833a4a32122ba2b91b4d3c95f40db5363e491`. The five-node Docker
preflight passed its kubeadm 3-control-plane/2-worker Cilium cluster,
control-plane loss, and recovery checks; it did not run nodemigrate. Both
single-node lanes built the utility and branch runtime, passed the source and
nodestore checkpoints, verified PV/PVC bindings and payloads, then failed
strict parity because the source CSI StatefulSet's `/dev` `dev-dir` volume was
absent on target. Its CSI data-root volume matched. Neither lane reached
return. Artifacts were downloaded once to
`/tmp/nodemigrate-36318123614-artifacts/`. This run used branch runtime, not
regular release `v0.8.0`.

The focused `nodeapiserver` quick-check
[36319639827](https://github.com/centerionware/not-k8s/actions/runs/36319639827)
passed at SHA `e37f4c0f`, including the StatefulSet hostPath schema-pruning
regression. This proves the built-in schema-pruning step retains the `/dev`
volume. A second nodeapiserver quick-check
[36319953565](https://github.com/centerionware/not-k8s/actions/runs/36319953565)
passed at SHA `e8502d46`, including the protobuf StatefulSet storage
round-trip regression; both isolated paths preserve the volume, while full
HTTP read/write behavior remains unverified. The current migration script
checks the volume immediately after each migration and before CSI fixture
reinstallation, then restores the source volume if that setup changes it.
`bash -n` and `git diff --check` pass; the dedicated migration rerun is pending.

The preceding focused Rust check is
[36315451874](https://github.com/centerionware/not-k8s/actions/runs/36315451874)
for nodemigrate SHA `17281efb42343ae2f934f681f57f031a59bc6016`. A focused
nodeapiserver protobuf regression is now covered by run `36319953565`. The
latest migration fixture changes have not yet been run.

Run [36307445772](https://github.com/centerionware/not-k8s/actions/runs/36307445772)
at SHA `d92e814911416403bba8b9cf02940fc23be1d140` passed the five-node
Docker/kubeadm/Cilium preflight and both utility/runtime builds. PVC identities,
PV bindings, and StatefulSet data passed at source and nodestore in both lanes;
both lanes reached `PASS stage=nodestore`. K3s then failed strict semantic
parity on controller-owned Lease fields, default `PriorityClass.globalDefault`,
and generated CSINode ownership. Upstream failed on those too, plus CoreDNS
bootstrap RBAC metadata and `CSIDriver.spec.preventPodSchedulingIfMissing`.
The last field was present in the Kubernetes 1.37.1 source API but missing
from the target's release-1.34 schema. The branch now preserves the field and
enforces its CSI registration policy; the initial API test exposed pruning's
failure to merge OpenAPI `allOf` properties, which is also fixed. Verification
on the updated SHA is pending. Logs are saved at
`/tmp/nodemigrate-36307445772/`.

The harness now lifecycle-normalizes the known controller leases and
Kubernetes default/bootstrap metadata, excludes node-generated `CSINode`
objects from byte-for-byte parity, and checks that each Node has a newly
registered hostpath CSI driver entry owned by its current Node UID. User
application Leases, custom RBAC, enabled PriorityClass defaults, durable
CSIDriver specs, and application rollout history remain strict. Local shell and
snapshot-filter checks pass; dedicated runtime validation is pending.

Commit [`de2b9ed9`](https://github.com/centerionware/not-k8s/commit/de2b9ed91210a4894bcdb0203bfd9320109335a4)
adds a narrow Kubernetes 1.37 `CSIDriver.spec.preventPodSchedulingIfMissing`
compatibility overlay to the 1.34-based API schema and teaches `nodescheduler`
to preserve and enforce that opt-in policy. The field is no longer a candidate
for snapshot normalization. Focused API schema, protobuf, projection, and
scheduler regression tests are in the branch. Component quick-check
[36309717348](https://github.com/centerionware/not-k8s/actions/runs/36309717348)
and a branch-runtime migration rerun
[36309717243](https://github.com/centerionware/not-k8s/actions/runs/36309717243)
were dispatched at SHA `de2b9ed91210a4894bcdb0203bfd9320109335a4`. The
quick-check failed in its unit-test step with exit code 101; its job log is
not yet retrievable because `gh` currently cannot connect to api.github.com.
The migration lanes are still running. The earlier migration run
[36308845021](https://github.com/centerionware/not-k8s/actions/runs/36308845021)
at SHA `7ba45556862d3ecd90cffe22cde4ac4d71a9f71b` failed in both lanes after
their migration steps and predates this API/scheduler fix; its logs are also
not yet retrievable during the API connection failure.

Run [36306098280](https://github.com/centerionware/not-k8s/actions/runs/36306098280)
at SHA `39ceeb189ac619988ddc96815611b6e4d1848198` passed the five-node
Docker/kubeadm/Cilium preflight and built both the utility and branch runtime.
Both migration lanes failed before completing a round trip. The fixed
StatefulSet fixture preserved ordinal 0/1 PVC identities, PV bindings, and data
at both source and nodestore checkpoints in both lanes. K3s then reported the
source-discovered `kube-system/metrics-server-77dbbf84b` ReplicaSet missing at
nodestore. Upstream Kubernetes failed strict source-to-nodestore parity;
reported differences include changing Leases, CoreDNS RBAC defaults,
PriorityClass defaults, `CSIDriver` scheduling behavior, and `CSINode` owner
references. Neither lane reached the return leg. Full logs are saved at
`/tmp/nodemigrate-36306098280/`.

The K3s missing-object failure was the source `metrics-server` ReplicaSet
identity being replaced by the current ReplicaSet generated by its Deployment
on the target. The harness now classifies that system-owned child by its owner
Deployment and checks that the target metrics-server rollout has a current
ReplicaSet. The snapshot-filter check passes; a migration rerun is pending.

The fixture issue was first observed in run
[36304940296](https://github.com/centerionware/not-k8s/actions/runs/36304940296):
it deleted ordinal 1's PVC, so the hostPath CSI provisioner deleted that
claim's `Delete`-policy PV before nodemigrate started. Nodemigrate itself did
not delete the PV. The fix is verified by the storage assertions in run
36306098280.


Run [36299240279](https://github.com/centerionware/not-k8s/actions/runs/36299240279)
at SHA `20648debff0f7f338c3e37303bbaf367ffa449a4` completed with a passed
five-node Docker/kubeadm/Cilium preflight and both utility/runtime builds;
both migration lanes failed during runtime checks. K3s passed its
nodestore-stage workloads (including Deployment scale and StatefulSet data),
then failed because its discovered-resource inventory lacked `nodes.metrics.k8s.io`
and `pods.metrics.k8s.io`. The APIService was Available, but metrics-server
logged an unknown-CA error scraping nodelet. The hostPath CSI driver registered
but reported a migrated volume handle missing from its volume list. Upstream
passed stage behavior and resource exposure, then failed strict same-identity
object parity across CoreDNS, workload replica fields, Leases, system RBAC and
PriorityClasses, CSI, Cilium, and other listed objects. Neither lane reached
the return leg. Full logs were downloaded once to
`/tmp/nodemigrate-36299240279/`.

The destination aggregated-API trust fix at SHA
`d2eebede56084347cbc8eeba03fb12336bed7e6e` passed the targeted
`quick-check` for `nodebootstrap,nodemigrate` in
[36297802743](https://github.com/centerionware/not-k8s/actions/runs/36297802743)
and the standalone nodemigrate crate tests and packaging check in
[36297839984](https://github.com/centerionware/not-k8s/actions/runs/36297839984).
A follow-up PV/PVC export regression test passed nodemigrate crate tests at
SHA `a2214765e134fbd7dc72d113f36dcc99b41c1530` in
[36298232980](https://github.com/centerionware/not-k8s/actions/runs/36298232980).
These checks prove crate-level behavior, not migration data-path safety.
The branch-runtime migration run
[36297966919](https://github.com/centerionware/not-k8s/actions/runs/36297966919)
at SHA `d2eebede56084347cbc8eeba03fb12336bed7e6e` completed with both lanes
failing after the branch runtime builds and five-node Docker preflight passed.
K3s forward migration completed and destination readiness passed. At the
nodestore checkpoint, ingress, namespace trust CA, non-root emptyDir, and
system-addon rollouts passed; the Deployment scale assertion failed. The
hostPath CSI driver registered after initial retries, but later PVC staging
still failed. Its metrics APIService reported `Available=True`, but
metrics-server could not scrape the nodelet kubelet because the kubelet
serving certificate was signed by an unknown CA. Upstream forward migration
passed the source and nodestore workload checks and exposed all 119
source-discovered listable API resources, then failed strict source-object
parity on same-identity field differences in workloads, system state, CSI,
and Cilium objects. Neither lane ran return migration or achieved a round
trip. Full logs:
`/tmp/nodemigrate-36297966919-k3s.log` and
`/tmp/nodemigrate-36297966919-kubernetes.log`.

The harness now prints Deployment, ReplicaSet, Pod, and event state if its
scale assertion fails. That change is in
[36299240279](https://github.com/centerionware/not-k8s/actions/runs/36299240279)
at SHA `20648deb`; in this run the scale assertion passed. Nodelet now logs the
full error chain for CSI volume mount failures;
the component check passed at SHA `aa96d6b5` in
[36299331009](https://github.com/centerionware/not-k8s/actions/runs/36299331009).
The active runtime run predates that nodelet change, so CSI status diagnostics
from it are still pending a runtime run on `aa96d6b5`.

Migration run
[36294950798](https://github.com/centerionware/not-k8s/actions/runs/36294950798)
at SHA `acf583bf` passed five-node preflight and both utility/runtime builds.
The nodeapiserver probe-auth fix is independently verified by targeted
quick-check [36294951013](https://github.com/centerionware/not-k8s/actions/runs/36294951013):
K3s metrics APIService then reported `Available=True` with reason `Passed`.
Both migration lanes stopped before migration because the harness exceeded the
OS command-line argument limit while passing large field-fingerprint JSON into
`jq`. The harness fix now streams each normalized object over stdin and stores
only identity/digest summaries in semantic snapshots. Shell syntax, inventory
comparator, and whitespace checks pass locally. Dedicated migration rerun is
pending; no source PV/PVC/data deletion or round-trip behavior was exercised by
that failed run. The prior upstream same-identity source-object differences
remain unresolved.

Run [36293700293](https://github.com/centerionware/not-k8s/actions/runs/36293700293)
at SHA `ad2ee6a016993c43effeab3d1aa83e9f6cf40f5e` completed with both
migration lanes failing. Five-node Docker/kubeadm/Cilium preflight and both
branch utility/runtime builds passed. Targeted nodelet quick-check
[36293700468](https://github.com/centerionware/not-k8s/actions/runs/36293700468)
passed. The new non-root, read-only-rootfs emptyDir fixture passed at source
and nodestore checkpoints in both lanes. K3s then failed because
`nodes.metrics.k8s.io` and `pods.metrics.k8s.io` were missing: metrics-server
was Ready and served its generated TLS cert, but its APIService discovery
probe got HTTP 403. This was fixed and confirmed in run `36294950798` as
described above.

Upstream passed source and nodestore API readiness, then failed strict source
object digest parity on same-identity objects including CoreDNS, CSI driver,
ReplicaSets/StatefulSets, Cilium/CSI node state, Leases, system RBAC, and
PriorityClasses. No return migration or round trip passed. The current
worktree improves this checker to report destination-only identities
separately and changed normalized field paths while still failing on every
changed or missing source object; it does not print object or Secret values.
Its expanded inventory-script check and next runtime run are pending. Full lane
logs are saved at `/tmp/nodemigrate-36293700293-{k3s,kubernetes}-ci.log` and
artifacts at `/tmp/nodemigrate-36293700293-artifacts/`.

Branch-runtime run [36285968999](https://github.com/centerionware/not-k8s/actions/runs/36285968999)
at SHA `879a0a618efe933d084023300460128da446da17` completed. Docker's
five-node kubeadm/Cilium preflight, both `nodemigrate` builds, and both branch
runtime builds passed. Both source lanes migrated to nodestore and passed API
readiness; K3s applied all 59 CRDs successfully. Both lanes then failed the
same nodestore checkpoint assertion: `POST .../pods/{name}/eviction` returned
404 where the fixture expects a PDB-protected 429. The upstream audit event
confirms response code 404 for the eviction request. The target REST dispatcher
had no eviction handler despite existing NodeRestriction checks and the
disruption controller's PDB status support. No return migration or round-trip
comparison ran. The completed lane logs and preflight log are in
`/tmp/nodemigrate-36285968999-artifacts/`; the K3s log is also at
`/tmp/nodemigrate-36285968999-k3s/nodemigrate-k3s.log`.

The feature branch now adds a `pods/eviction` handler that checks matching
PDB selectors and returns `TooManyRequests` when `disruptionsAllowed` is zero,
then uses graceful Pod deletion when allowed. It also invokes NodeRestriction
for node identities and adds focused selector/status tests. These changes have
passed targeted `nodeapiserver` quick-check
[36287345872](https://github.com/centerionware/not-k8s/actions/runs/36287345872).
Migration run [36287345921](https://github.com/centerionware/not-k8s/actions/runs/36287345921)
then passed the PDB 429 assertion in both lanes and continued into StatefulSet
volume-claim-template scale checks. It failed in both lanes because the
temporary ordinal-1 PV was not deleted after its PVC was removed. The cause is
confirmed in `nodecontroller::pv_binder`, whose documented scope had no release
or reclaim handling. A claim-release reconciler is now in the worktree; its
first revision passed focused `nodecontroller` quick-check
[36288813455](https://github.com/centerionware/not-k8s/actions/runs/36288813455)
but the migration rerun below exposed the need to mark a `Delete` PV Released
before deletion. The eviction path still
does not reserve allowance atomically across concurrent requests, so full PDB
concurrency semantics remain unverified.

Migration run [36288813404](https://github.com/centerionware/not-k8s/actions/runs/36288813404)
at SHA `2ea1a793` passed preflight, builds, API readiness, and StatefulSet
ordinal-0 data checks in both lanes. Both lanes still failed after deleting the
temporary ordinal-1 PVC because its PV remained. CSI provisioner logs directly
showed `shouldDelete is false: PersistentVolumePhase is not Released`. The
worktree now patches `status.phase=Released` before the preconditioned PV
delete. This reclaim case is only the test's explicit deletion of its temporary
ordinal-1 PVC; migration itself must preserve source PVs, PVCs, backing storage,
and data. The first quick-check of this ordering change failed to compile
because a claimRef borrow remained live across the PV status update. The
namespace/name/UID are now copied before the update; targeted quick-check and
another branch-runtime migration are pending. No return migration or parity
comparison passed. Full artifacts:
`/tmp/nodemigrate-36288813404-artifacts/nodemigrate-{k3s,kubernetes}-36288813404/`.

On `0d519f85a540c59edb032b7724826399101ba720`, focused `nodecontroller`
quick-check [36290302028](https://github.com/centerionware/not-k8s/actions/runs/36290302028)
passed after copying the claimRef identity before the PV update. The immediately
preceding migration run [36290028538](https://github.com/centerionware/not-k8s/actions/runs/36290028538)
used the earlier `42d42cfb` SHA: five-node Docker preflight passed, but both
runtime build jobs failed to compile, so neither lane performed migration or
changed a PV. The corrected-SHA rerun [36290458212](https://github.com/centerionware/not-k8s/actions/runs/36290458212)
at `863ea3ba27d75a201f13bf1964ea6bca0171c643` has passed the five-node Docker
preflight and both `nodemigrate` and branch-runtime builds; the K3s and upstream
migration steps are still running. This run also checks PV/PVC semantic state
at the source, target, and returned-source checkpoints. No result is recorded
until both lanes finish.

Release-backed migration run [36283329273](https://github.com/centerionware/not-k8s/actions/runs/36283329273)
at SHA `786e95577686f1fd5ed08a94f76ade48861314b4` fetched the exact regular
v0.8.0 runtime. The five-node Docker/kubeadm/Cilium preflight passed in 7m32s.
Both migration utility builds and release fetches passed. The K3s lane failed
after 18m02s; the upstream lane failed after 48m23s. In both lanes, the
released nodeapiserver rejected the same three Gateway API CRDs due to CEL
map/object typing and cost estimation; dependent Gateway objects could not be
restored. Upstream also had a CertificateRequest webhook call failure and a CSR
HTTP 500; these need branch-runtime diagnosis. PriorityClass import ordering
is fixed: the prior Pod admission 403 did not recur. Both sources recovered
after rollback and both protected exports were retained. No semantic parity or
return migration passed. The targeted
[quick-check 36283329275](https://github.com/centerionware/not-k8s/actions/runs/36283329275)
passed for `nodeapiserver` and `nodemigrate`. Migration artifacts:
`/tmp/nodemigrate-36283329273-k3s/` and `/tmp/nodemigrate-36283329273-k8s/`;
preflight artifact: `/tmp/nodemigrate-36283329273-docker/`.

The release test confirms v0.8.0 behavior. Test the branch runtime next to
exercise the CEL checker fix and verify the priority-ordering repair with all
current component fixes.

Release-backed migration run [36279843865](https://github.com/centerionware/not-k8s/actions/runs/36279843865)
at SHA `d609ec4d66dfce13283d0ee4c1d39a5ecce10d8d` fetched the exact regular
runtime asset `notk8s-0.8.0-linux-x86_64-release`. Docker's five-node
kubeadm/Cilium preflight passed in 6m11s, and both targeted `nodemigrate`
release builds passed. K3s migration failed after 14m26s; upstream Kubernetes
migration failed after 45m47s. Both source checkpoints passed, then imports
failed and rolled back while retaining the protected export. The K3s lane
rejected three Gateway API CRDs due to `nodeapiserver` CEL map/object handling,
then could not restore two Gateway objects. The upstream lane had the same CRD
failures, a Pod admission 403 because PriorityClass import ordering was not
dependency-aware, a CertificateRequest rejected when the cert-manager
mutating webhook could not be reached, a CSR rejected while decoding its
`ExtraValue`, and two unavailable Gateway objects. No semantic parity,
return migration, or round-trip gate passed. The importer now ranks
PriorityClasses before workloads; focused quick-check and a new release-backed
run are pending. Full lane artifacts:
`/tmp/nodemigrate-36279843865-artifacts/nodemigrate-{k3s,kubernetes}-36279843865/`.

The Docker-only preflight [36278826637](https://github.com/centerionware/not-k8s/actions/runs/36278826637)
passed at SHA `becc80a1de8cbb6c1204e3d27d62c39cb332f4a3` in 5m06s. It created
five isolated Docker nodes with distinct network/mount namespaces, machine
IDs, BPF pins, and persistent volumes; formed a kubeadm cluster with three
control planes and two workers; installed Cilium 1.20.2; verified all five
Nodes Ready; stopped cp-1 and verified the remaining control plane kept the
API available; restarted cp-1 and verified all Nodes recovered. This validates
the five-node Kubernetes/Cilium simulation only. Migration was not run in this
Docker-only invocation. Log: `/tmp/nodemigrate-36278826637-job.log`; artifact:
`/tmp/nodemigrate-36278826637-artifact/nodemigrate-docker-preflight-36278826637/nodemigrate-docker-preflight.log`.

Release-backed migration run [36279222996](https://github.com/centerionware/not-k8s/actions/runs/36279222996)
at SHA `8e630ce3103b2eaf510a7212d2564ca391fb1e49` built `nodemigrate` and
fetched the exact regular runtime asset `notk8s-0.8.0-linux-x86_64-release`
for both K3s and upstream lanes. The Docker preflight passed again. Both
migration lanes installed their source fixtures and passed source checks,
including Ingress and API CA checks, then stopped before calling nodemigrate:
the PDB/eviction assertion sent YAML through `kubectl create --raw`, which
forwards the bytes unchanged and received a JSON parse error. This was a
fixture failure, not evidence of migration behavior. The request is now built
as JSON; release-backed rerun is pending. Full run log:
`/tmp/nodemigrate-36279222996-job.log`; lane logs:
`/tmp/nodemigrate-36279222996-artifacts/nodemigrate-{k3s,kubernetes}-36279222996/`.

The Docker-only attempt [36271904146](https://github.com/centerionware/not-k8s/actions/runs/36271904146)
used SHA `9b458e57836039745fee90bb34d0a0ceeb44ec4a`. The image build and
five-container isolation checks passed. kubeadm then failed image pulls on all
five nodes. The captured logs show containerd `2.2.1` warning
`Unpack configuration not supported, skipping` for `linux/amd64` with
`snapshotter=native`, followed by `no unpack platforms defined`. The workflow
skipped both migration lanes by design. The harness now appends and asserts a
containerd Transfer API `unpack_config` mapping for `linux/amd64` and the
native snapshotter. Shell syntax and focused config-pattern checks passed
locally; the corrected Docker-only rerun is pending. Log:
`/tmp/nodemigrate-36271904146-job.log`.

Docker-only run [36272360238](https://github.com/centerionware/not-k8s/actions/runs/36272360238)
at SHA `9c0f58fef4b8eceb38a53f929210bcaae70e73b2` passed the image build and
all five containers' namespace, CRI, BPF, storage, and network checks. It then
stopped before restarting containerd because nested single quotes broke the
lookup passed through the outer single-quoted `bash -ec` argument. Run
[36272577668](https://github.com/centerionware/not-k8s/actions/runs/36272577668)
reproduced the same quoting issue after the literal lookup was introduced; the
appended config was printed in failure diagnostics. The lookup and value
assertions now use escaped double quotes throughout. Shell syntax and a local
nested-shell fixture pass. Docker-only run
[36272793413](https://github.com/centerionware/not-k8s/actions/runs/36272793413)
at SHA `896bea4f2f55db1c7824aa23d9f6d2ce656df5e4` then passed the unpack
mapping and all five node-container checks. kubeadm initialized cp-1, joined
cp-2/cp-3 as stacked-etcd control planes, and joined worker-1/worker-2.
Installing Cilium `1.20.2` with Helm timed out after ten minutes because its
agents and Envoy could not start: containerd reported `/sys/fs/bpf` was neither
a shared nor slave mount for nested Cilium Pods. The timeout was secondary to
the Docker node's private bpffs mount propagation. The diagnostics added in
`1f46f848` captured the kubelet/containerd error. The harness now marks each
node's private bpffs mount recursively shared inside that node's own mount
namespace and asserts its mountinfo propagation flag before starting kubeadm.
This fix and the Cilium rerun remain unverified. No Cilium readiness, API
survival under control-plane loss, restart recovery, or migration check passed.
Log: `/tmp/nodemigrate-36273723872-job.log` and artifact
`/tmp/nodemigrate-36273723872-artifact/nodemigrate-docker-preflight.log`.

Docker-only run [36274669206](https://github.com/centerionware/not-k8s/actions/runs/36274669206)
at SHA `3d6787dd6f5869b24fd55252addee857ffd3d72d` failed before the node
containers were created: the mountinfo assertion's embedded `awk` program
expanded `$5` in the wrong shell. The assertion now uses a POSIX `case` check
on the `/sys/fs/bpf` mountinfo line instead. `bash -n`, `sh -ec` fixture, and
`git diff --check` pass; another Docker-only run is pending. Log:
`/tmp/nodemigrate-36274669206-job.log`.

Docker-only run [36274819916](https://github.com/centerionware/not-k8s/actions/runs/36274819916)
at SHA `610a1f050937e69d7e5ee9db1c8414f1ab49405c` reached node isolation but
the inner `sh -ec` body again closed early because an apostrophe in the
diagnostic text broke the outer single-quoted argument; the runner then
expanded `$mountinfo_line` under `set -u`. The diagnostic wording now avoids
apostrophes. Shell syntax and mountinfo fixture checks pass; a new Docker-only
run is pending. Log: `/tmp/nodemigrate-36274819916-job.log`.

Docker-only run [36274980364](https://github.com/centerionware/not-k8s/actions/runs/36274980364)
at SHA `3f82684a1a0e6185296c13ab1ced14a54952f300` reached kubeadm/Cilium but
still failed with `/sys/fs/bpf` reported not shared inside nested Pods. The
first follow-up hypothesis was that the old check found a lower overmounted
entry, but [run 36276058940](https://github.com/centerionware/not-k8s/actions/runs/36276058940)
later showed that the bpffs entry itself was shared. The Cilium error must
therefore be investigated across the node-to-Pod mount handoff. The failed
follow-up's two-row `findmnt` result is recorded below. Log:
`/tmp/nodemigrate-36274980364-job.log` and artifact
`/tmp/nodemigrate-36274980364-artifact/nodemigrate-docker-preflight.log`.

Docker-only run [36276058940](https://github.com/centerionware/not-k8s/actions/runs/36276058940)
at SHA `d55e1dcdb543add420a5ab9b942ca015c749d50a` failed in 50 seconds before
kubeadm. The `findmnt` query returned two mounts at `/sys/fs/bpf`: a private
tmpfs and a shared bpffs. The bpffs entry itself was shared; the single-value
assertion incorrectly treated both rows as one value. This disproves the
earlier guess that the mountinfo check merely found a lower private mount
record. The assertion was corrected to filter by filesystem type `bpf`.
`/tmp/nodemigrate-36276058940-job.log` and artifact
`/tmp/nodemigrate-36276058940-artifact/nodemigrate-docker-preflight.log`.

Docker-only run [36276280017](https://github.com/centerionware/not-k8s/actions/runs/36276280017)
at SHA `06ff3d3f9c9b936c74af6be636904ad90a6ce694` passed that assertion and
reached Cilium, and no longer reported the `/sys/fs/bpf` propagation error.
Instead, containerd rejected Cilium's default cgroup v2 root at
`/run/cilium/cgroupv2` as not shared or slave. This confirms that removing the
overlapping bpffs tmpfs fixed that mount handoff. The worktree now mounts a
node-local cgroup2 filesystem at Cilium's default path, marks it recursively
shared, and checks that mount before kubeadm. Job log:
`/tmp/nodemigrate-36276280017-job.log`; artifact:
`/tmp/nodemigrate-36276280017-artifact/nodemigrate-docker-preflight-36276280017/nodemigrate-docker-preflight.log`.
This direct cgroup2 mount was a pre-worktree proposal and is superseded by the
latest run below.

Docker-only run [36278041301](https://github.com/centerionware/not-k8s/actions/runs/36278041301)
at SHA `ba2d2ff4058fa8e2a18951d5accb519c141ddd08` passed all five node
isolation checks, including shared `/sys/fs/bpf` and `/run/cilium/cgroupv2`
mounts, and formed the five-node kubeadm cluster. Cilium progressed past both
paths, then containerd rejected `/var/run/netns` because its resolved mount
was the private `/run` tmpfs. The worktree marks each node-local `/run` mount
recursively shared and asserts its propagation before kubeadm; this next
change is unverified. Job log: `/tmp/nodemigrate-36278041301-job.log` and
artifact:
`/tmp/nodemigrate-36278041301-artifact/nodemigrate-docker-preflight-36278041301/nodemigrate-docker-preflight.log`.

Earlier Docker-only attempts are archived at these logs:
`/tmp/nodemigrate-36272360238-job.log` and
`/tmp/nodemigrate-36272577668-job.log`.

Migration run [36260417450](https://github.com/centerionware/not-k8s/actions/runs/36260417450)
at SHA `8c79470de60f288fc113db7b7b8c45da048b6ad7` is now terminal. The
upstream lane failed during return import after cert-manager webhook timeouts
and a PV 404; K3s was cancelled at 20:50 UTC while its migration step was still
running. Neither lane proved a full round trip. The five-node preflight in
that run passed only the earlier container-isolation checks. No regular
build or full e2e gate was run.

## Latest branch-runtime result

Migration run [36258945785](https://github.com/centerionware/not-k8s/actions/runs/36258945785)
used SHA `bb6d20a99d31c6e015dee5bf65d1188ff3bf7e43`, with the branch runtime.
Docker five-node preflight and both `nodemigrate` and combined-runtime builds
passed. In K3s+Cilium, import failed on a CertificateRequest while the
destination cert-manager webhook was still unavailable; the importer's five
three-second retries expired before Cilium/cert-manager startup completed.
Source rollback and protected export retention passed. In upstream
Kubernetes+Cilium, forward import and the target stage completed, then the
semantic fixture comparison rejected two representation changes: generated
`kube-root-ca.crt` annotation metadata (its CA bundle is checked separately
against the active API CA) and optional `apiVersion`/`kind` fields in nested
StatefulSet PVC templates. No reverse migration ran in either lane. Artifacts:
`/tmp/nodemigrate-36258945785-artifacts/nodemigrate-{k3s,kubernetes}-36258945785/`.

The current branch adds bounded retries for transient API/webhook and
transport failures, with permanent API rejections failing immediately. It
also gives the application fixture a dedicated semantic normalizer that
excludes only the generated namespace root CA ConfigMap and removes optional
TypeMeta from PVC templates. The upstream lane passed the nodestore target
checkpoint, then failed on the return import after five minutes: three
cert-manager resources timed out reaching the webhook ClusterIP, and one PV
restore returned HTTP 404. The PV request path was not present in that run's
error, so its cause remains unconfirmed. The K3s lane was later cancelled before completion.
Commits `058fafa8` and `434c9acb` add return-leg service diagnostics and include
the destination API request path in restore errors. Shell syntax, jq probe
selection, and `git diff --check` passed locally. Focused nodemigrate crate
tests passed in [36263787654](https://github.com/centerionware/not-k8s/actions/runs/36263787654)
at `434c9acb`; migration-workflow validation passed in
[36263787652](https://github.com/centerionware/not-k8s/actions/runs/36263787652).
Commit `3f7c8873` adds reverse-cutover rollback and checks recovered nodestore
state in the integration harness. Focused nodemigrate tests
[36264669564](https://github.com/centerionware/not-k8s/actions/runs/36264669564)
and migration-workflow validation
[36264669587](https://github.com/centerionware/not-k8s/actions/runs/36264669587)
passed at that SHA; runtime migration verification is pending.
The `nodemigrate` quick-check
[36260417435](https://github.com/centerionware/not-k8s/actions/runs/36260417435)
passed at `8c79470de60f288fc113db7b7b8c45da048b6ad7`. Branch-runtime migration
[36260417450](https://github.com/centerionware/not-k8s/actions/runs/36260417450)
ran at `8c79470de60f288fc113db7b7b8c45da048b6ad7`; upstream failed as
described above and K3s was later cancelled before completion. No regular
full build or full e2e gate was dispatched.

The migration fixture now compares deployed Helm release records at each
checkpoint: release identity, chart/app version, revision, and digests of
computed values and rendered manifests. Local shell syntax, snapshot-filter,
diff, and stubbed Helm capture checks passed. On PR SHA `32e8ccb2`, migration
workflow validation [36265673005](https://github.com/centerionware/not-k8s/actions/runs/36265673005)
and focused `nodemigrate` crate tests [36265672968](https://github.com/centerionware/not-k8s/actions/runs/36265672968)
passed. The active migration run above predates this addition; runtime
verification remains pending. At 2026-09-26 19:21 UTC, its upstream lane had
failed and its K3s lane remained in progress. No regular build or full e2e was
run; the migration jobs were skipped on the pull-request validation event.

The newest fixture change adds a PDB-protected nginx eviction check at all
three checkpoints and includes the PDB in durable-state comparison. Local
shell syntax and helper checks passed, and PR workflow validation
[36265900635](https://github.com/centerionware/not-k8s/actions/runs/36265900635)
passed at `2ee43f2a`. The active runtime run predates this addition; dedicated
PDB behavior verification remains pending. At 2026-09-26 19:24 UTC, that run's
K3s lane was still in progress.

The fixture now also exercises Deployment `/scale` at each checkpoint (two
available replicas, then back to one). Local shell syntax and helper checks
passed. Migration workflow validation
[36266043924](https://github.com/centerionware/not-k8s/actions/runs/36266043924)
and focused `nodemigrate` tests
[36266043923](https://github.com/centerionware/not-k8s/actions/runs/36266043923)
passed at `28d04774`. Runtime verification remains pending because the active
K3s migration run predates this fixture change.

Commit `428286d9` updates the eviction assertion to accept Kubernetes' normal
`TooManyRequests` status spelling as well as HTTP 429. Local shell/helper
checks and migration workflow validation
[36266303223](https://github.com/centerionware/not-k8s/actions/runs/36266303223)
passed. At 2026-09-26 19:31 UTC, run `36260417450` still reported its K3s
migration step in progress; GitHub later marked that step cancelled at 20:50
UTC, as recorded above.

Commit `c3be42bc` changes the import retry classifier so 404s for built-in
resources fail immediately, while source-CRD custom resources retain bounded
404 retries for establishment races. The failing PV's destination request path
will now be reported without a five-minute wait. Focused nodemigrate tests
[36266754129](https://github.com/centerionware/not-k8s/actions/runs/36266754129)
and migration workflow validation
[36266754130](https://github.com/centerionware/not-k8s/actions/runs/36266754130)
passed. The root cause and successful runtime recovery remain unverified; the
older K3s lane was still active at 2026-09-26 19:40 UTC.

The fixture now captures `kubectl api-resources --verbs=list -o name` at all
three checkpoints and requires the target and returned source to retain every
source-discovered listable resource, even when it has no objects. It prints
the exact inventory and missing resource names on failure. Local shell syntax,
snapshot, Helm, and API inventory checks passed. On PR head
`a211413a5034dbbf342caa07e1ee739f4f239b05`, migration workflow validation
[36267290452](https://github.com/centerionware/not-k8s/actions/runs/36267290452)
and focused `nodemigrate` tests
[36267290457](https://github.com/centerionware/not-k8s/actions/runs/36267290457)
passed. Runtime verification is pending: the older dedicated migration run
[36260417450](https://github.com/centerionware/not-k8s/actions/runs/36260417450)
used SHA `8c79470de60f288fc113db7b7b8c45da048b6ad7`; its upstream lane failed
after the return import webhook timeouts and PV 404, while its K3s `Run
migration` step remained `in_progress` at 2026-09-26 19:51 UTC. This run
predates the API inventory assertion and cannot verify it. No regular build or
full e2e gate was dispatched.

The API inventory assertion now compares normalized object hashes as well as
resource and object identities on the source-to-nodestore leg; round-trip
semantic-state comparison already checks those hashes. This makes target-side
spec/data changes fail the migration fixture, with a diff of affected object
identities. Local shell syntax, API inventory positive/negative cases,
snapshot-normalization, Helm-state checks, and `git diff --check` passed at
`cccbc4afbd712ba9bdad6be783d86b4c3f983719`. PR validation and a runtime run
for branch head `e0d2247e1f75786282dd8e485a7d231d480f2018` passed in
[36267765013](https://github.com/centerionware/not-k8s/actions/runs/36267765013).
A runtime run with this stronger comparison is pending. The active older run
`36260417450` remains on its previously recorded SHA and is not duplicated.
Focused `nodemigrate` crate tests and the release-packaging policy check also
passed at branch head `e0d2247e1f75786282dd8e485a7d231d480f2018` in
[36267764895](https://github.com/centerionware/not-k8s/actions/runs/36267764895)
and [36267765074](https://github.com/centerionware/not-k8s/actions/runs/36267765074),
respectively. The latter checked release policy only; no release was
published.

The fixture now scales the StatefulSet from one ordinal to two and back at
every source, nodestore, and returned-source checkpoint. It checks both Pods
and template-created claims, writes unique data to ordinal 1, retains its PVC,
PV binding, and payload through scale-down/up, and verifies both ordinals retain
their data and PVC identities. It also changes `minReadySeconds`, waits for StatefulSet
`observedGeneration`, and restores the original setting. Local shell syntax, API inventory/object-hash checks,
snapshot-normalization, Helm-state checks, and `git diff --check` passed at
`a936dc7e4b625ee67c49429f11a1c1778372d1c9`. PR validation and cluster runtime
verification are pending; the active migration run is on an older SHA and
cannot cover this addition.

The exporter now reports every listable source API resource with its selected
object count, and reports skipped resource/object counts with explicit
lifecycle reasons (read-only/status, regenerated by destination controllers,
or refreshed runtime state). Focused tests cover the skip-reason mapping. On
SHA `39890702faf4815b022bd9419472e509b6f1076e`, focused `nodemigrate` crate
tests passed in [36268585399](https://github.com/centerionware/not-k8s/actions/runs/36268585399),
migration workflow validation passed in
[36268585450](https://github.com/centerionware/not-k8s/actions/runs/36268585450),
and release-policy validation passed in
[36268585440](https://github.com/centerionware/not-k8s/actions/runs/36268585440).
No release was published. Runtime migration of the per-resource report is
pending; the still-active run `36260417450` predates this change.

The Docker five-node preflight now builds a real upstream kubeadm 1.35 cluster
with three stacked-etcd control planes and two workers, installs Cilium, checks
all five Nodes and Cilium agents Ready, then stops one control plane and
verifies API readiness through a surviving control plane before restoring the
fifth Node. The job timeout is 60 minutes for this cluster setup. This checks
whether Docker models the topology and control-plane failure needed by the
multi-node migration gate; it does not run nodemigrate or prove migration
parity. `bash -n` and `git diff --check` pass locally. The Docker-only attempt
[36269422051](https://github.com/centerionware/not-k8s/actions/runs/36269422051)
at SHA `e49d4b602e5a5d2d87b1d2a6c8150148eaa2542a` failed in `Build node image`
after about 21 seconds, before the kubeadm probe. `docker_only=true` skipped the
migration job. The job log showed `curl: (77)` because
`/etc/ssl/certs/ca-certificates.crt` was missing while fetching the Kubernetes
signing key; GPG then reported no valid key data. The image now installs
`ca-certificates` explicitly. No cluster or migration behavior was tested by
that run; the Docker-only rerun at SHA `4c5af2adb01fb876409462f3054de1f38923f8e8`
fixed the image build and passed five-container systemd, namespace, CRI, BPF,
storage, and peer-network checks. It then failed because `modprobe` was absent
while configuring kubeadm prerequisites. Adding `kmod` exposed that the
container does not include modules for the runner's `6.17.0-1022-azure` kernel.
The script now loads `overlay` and `br_netfilter` on the Docker host before
starting privileged containers; the redundant container `kmod` dependency was
removed. The Docker-only rerun is pending. No kubeadm cluster or Cilium result
has passed yet.
The rerun [36270087374](https://github.com/centerionware/not-k8s/actions/runs/36270087374)
at SHA `2f35443e3b5b6e141aa03b47bf5e0fc041fe0d0d` successfully loaded the host
modules and repeated all five container isolation checks, then exited 32 while
configuring kubeadm prerequisites. The next diagnostic run
[36270240134](https://github.com/centerionware/not-k8s/actions/runs/36270240134)
showed that `sysctl --system` completed and `swapoff -a` returned 32 inside
`cp-1`. Since all privileged containers share the runner kernel and swap, the
harness now handles swap on the ephemeral GitHub runner before launching the
containers. Run [36270390355](https://github.com/centerionware/not-k8s/actions/runs/36270390355)
confirmed a 3 GiB `/swapfile` with zero used; the next run logs memory/swap,
disables swap on the runner, and verifies it is off before kubelet setup. No
kubeadm or Cilium checks have passed yet.
Run [36270521782](https://github.com/centerionware/not-k8s/actions/runs/36270521782)
confirmed the 15 GiB runner had 3 GiB of unused swap, which was disabled; host
module loading, sysctl setup, and all five isolation checks passed. Setup then
failed writing `/etc/containerd/config.toml` because the directory was absent.
The probe now creates `/etc/containerd` before generating its configuration.
The rerun [36270640042](https://github.com/centerionware/not-k8s/actions/runs/36270640042)
passed host swap setup, containerd configuration, and reached `kubeadm init`,
which failed its SystemVerification check because `modprobe` was absent from
the node image. The image now restores `kmod` and mounts the runner's
`/lib/modules` read-only into each node. The runner does not provide the
`configs` module for its kernel, so the script now requires and mounts the
matching `/boot/config-$(uname -r)` read-only for kubeadm's kernel-config
inspection. Cluster and Cilium setup remain unverified.
Run [36271060218](https://github.com/centerionware/not-k8s/actions/runs/36271060218)
confirmed kubeadm passed SystemVerification, generated the control-plane
certificates/manifests, and started kubelet. It then timed out while waiting for
the API server to create the bootstrap admin binding. The preflight now collects
kubelet/containerd journals, CRI task/container state, and systemd-container
logs on all five nodes before cleanup when `kubeadm init` fails.
Run [36271304303](https://github.com/centerionware/not-k8s/actions/runs/36271304303)
captured the cause: containerd repeatedly failed to mount static-pod rootfs
layers with OverlayFS `invalid argument`, so the API server never started.
The test node's CRI snapshotter is now set to containerd's `native` file-copy
snapshotter to avoid nested OverlayFS inside Docker. The next kubeadm/Cilium
preflight is pending.
Run [36271711545](https://github.com/centerionware/not-k8s/actions/runs/36271711545)
failed before restarting containerd because the exact-value replacement did
not match the generated TOML quoting. The current command replaces `overlayfs`
only on `snapshotter =` lines, accepts either quote style, and prints the
snapshotter entries if its assertion fails.
Run [36271561812](https://github.com/centerionware/not-k8s/actions/runs/36271561812)
confirmed the node setup reached containerd configuration; the sed expression
for changing its snapshotter then broke the nested shell quoting. The harness
now uses a simpler exact-value replacement and checks the generated setting
before restarting containerd.

## Earlier branch-runtime attempt

The rerun after identity normalization,
[36257337049](https://github.com/centerionware/not-k8s/actions/runs/36257337049),
used SHA `7c5f2935146647c045f3f27d0d3dc9317ebfc9ed`. Docker preflight, both
nodemigrate/combined-runtime builds, and both source and target (`nodestore`)
stage checks passed. The retention assertion found two source K3s system
add-on ReplicaSets (`kube-system/coredns-54bf7cdff9` and
`kube-system/local-path-provisioner-69879d7dd7`) that the destination
controllers replaced, plus a source ClusterTrustBundle that the destination
serves through another version of the same API group/kind/object. The fixture's
own Deployment and StatefulSet rollout-history checks passed at target. The
next harness update compares Kubernetes identities by API group/kind/name
(version conversion is not a new object) and classifies only these two
controller-generated system add-on ReplicaSets as regenerated, while
verifying each present Deployment is available and has a current ReplicaSet.
Arbitrary ReplicaSets and fixture rollout history remain checked. CI rerun is
pending. Artifacts:
`/tmp/nodemigrate-36257337049-artifacts/`.

Migration run [36257337049](https://github.com/centerionware/not-k8s/actions/runs/36257337049)
uses SHA `7c5f2935146647c045f3f27d0d3dc9317ebfc9ed` and the branch runtime;
the Docker preflight passed and both migration lanes reached the target. The
run failed at object identity retention as described above. The harness checks
durable source API object identities and stable fixture state; it also
classifies CiliumEndpoint/CiliumIdentity as regenerated runtime state while
retaining CiliumNode in the migratable inventory. Local snapshot checks cover
the classifications.

Run [36256671367](https://github.com/centerionware/not-k8s/actions/runs/36256671367)
used SHA `7c5f2935146647c045f3f27d0d3dc9317ebfc9ed` against the latest regular
release, v0.8.0. Docker preflight, utility builds, and both source fixtures
passed. The v0.8.0 K3s target retained a Kubernetes API endpoint of
`127.0.0.1`, leaving Cilium and cert-manager without running Pods; upstream
failed CertificateRequest admission because its webhook was unreachable and
CSR import because of the known `ExtraValue` codec error. Both sources
recovered and retained their protected exports. Neither target-stage checks
nor the object-retention comparison ran. This is release-baseline evidence,
not a result for the branch runtime fixes. Artifacts:
`/tmp/nodemigrate-36256671367-artifacts/`.

Branch-runtime run [36255128061](https://github.com/centerionware/not-k8s/actions/runs/36255128061)
used SHA `4984eb2e8a285f284c8bbbb9581362cdd61d0f6c`. Both lanes passed source
and target (`nodestore`) stage checks, including the Ingress route/spec and
fixture workload/storage assertions. They then failed the all-object exact
fingerprint comparison before reverse migration. Its diff mixed target-added
controller/Cilium runtime objects and API-server-defaulted or controller-
updated hashes with durable source identity, so it did not establish a
migration data loss. Harness comparison was corrected on `7c5f2935`; branch
runtime rerun is in progress. Logs:
`/tmp/nodemigrate-36255128061-artifacts/`.

Run [36251890971](https://github.com/centerionware/not-k8s/actions/runs/36251890971)
used SHA `16c90a9721dd1f0bbcdc1a11723d7438d173b49f`. Both lanes passed the
source checks and target static hostPath/CSI data reads. The `nodeapiserver`
SPDY port-forward fix is effective in runtime: both Gateway probes returned
the nginx HTTP 200 page. Both Ingress probes returned HTTP 404; `describe`
showed an IngressClass but no host rules. The cause is not established because
raw source/target Ingress JSON and the IngressClass controller were not
captured. No target checkpoint, return migration, or parity comparison passed.
Focused `nodeapiserver` check [36251889485](https://github.com/centerionware/not-k8s/actions/runs/36251889485)
passed. Logs:
`/tmp/nodemigrate-36251890971-artifacts/nodemigrate-{k3s,kubernetes}-36251890971/`.

Harness commit `19e1cae6` added Ingress spec diagnostics; the assertion was
initially only called after source fixture installation. Run
[36253413938](https://github.com/centerionware/not-k8s/actions/runs/36253413938)
finished with both migration jobs failing at target Ingress routing after
successful utility/runtime builds and Docker preflight. Retrieved logs show
the target Ingress has its host but has lost the embedded `http.paths` backend;
the IngressClass controller is correct. This confirms a nodeapiserver
protobuf-codec defect. A fix for embedded `IngressRuleValue`, a codec
round-trip regression, and moving the assertion into every `verify_stage` are
Focused check [36255128063](https://github.com/centerionware/not-k8s/actions/runs/36255128063)
passed at `4984eb2e`. Logs from the preceding run:
`/tmp/nodemigrate-36253413938-artifacts/nodemigrate-{k3s,kubernetes}-36253413938/`.
No general build or e2e gate was run.

Migration run [36250505911](https://github.com/centerionware/not-k8s/actions/runs/36250505911)
used SHA `4d28a58886be2ced6f2c22626c4b370d8aa4bef3`. Docker five-node
preflight and both utility/runtime builds passed. Both K3s+Cilium and upstream
Kubernetes+Cilium lanes reached the target API and passed static PVC binding
and the static+CSI `migration-data-check` data reads. They failed at target
Ingress/Gateway traffic checks because `nodeapiserver` rejected
`kubectl port-forward` SPDY requests with no initial ports query. No return
migration or full parity ran. Logs and artifacts are under
`/tmp/nodemigrate-36250505911-artifacts/nodemigrate-{k3s,kubernetes}-36250505911/`.

Focused `nodelet` quick-check [36250504453](https://github.com/centerionware/not-k8s/actions/runs/36250504453)
passed at this SHA, and both lanes confirmed the hostPath PV data read. The
`nodeapiserver` fix for valid SPDY requests without query ports is committed
next; its focused component check and another dedicated migration run are
pending. No regular `build.yml` or full e2e gate was run.

Migration run [36241224151](https://github.com/centerionware/not-k8s/actions/runs/36241224151)
used SHA `606f46f624007dfd6215be26b97e54817fe020af`. Docker preflight, both
runtime builds, both source fixtures, and the target's Node/Cilium/Gateway/
DaemonSet/Deployment/standalone Pod/CA/CSI-readiness checks passed. Both lanes
then failed at the StatefulSet CSI mount. Captured nodelet and CSI logs show
`NodeStageVolume` returned `NotFound` because the imported CSI volume handle
was missing from the target hostpath driver's volume inventory. Source and
target PV handles were identical and node affinity matched; the import copied
CSI metadata without transferring payload or provider state. This is now a
confirmed nodemigrate storage migration defect. No semantic checkpoint,
reverse migration, or round trip passed. Artifacts:
`/tmp/nodemigrate-36241224151/`.

Migration run [36239708922](https://github.com/centerionware/not-k8s/actions/runs/36239708922)
used SHA `64fdb6572639b53d51b5d0c8fc2ddeaac7dc09af`. Docker preflight and
both nodemigrate/branch-runtime builds passed. Both source fixtures passed,
and the target reached the same StatefulSet readiness failure after passing
the Node, Cilium, Deployment, Gateway, standalone Pod, CA, and CSI-readiness
checks. The generation fix remains confirmed (`generation=1`,
`observedGeneration=1`). At failure, the StatefulSet pod was Pending with
`waiting for CSI volume(s) to be mounted: state`; its claim/PV were Pending.
The PV required `topology.hostpath.csi/node In [runnervmtr4k5]`, matching the
target Node label exactly, so the earlier `FailedScheduling` affinity events
do not explain the final CSI mount wait. This run predates nodelet/CSI log
capture. The latest branch now records source and target fixture PV/PVC specs
and captures nodelet, scheduler, and hostpath CSI logs on failures; a rerun is
needed to localize the mount failure. No semantic checkpoint, return
migration, or round trip passed. Saved artifacts:
`/tmp/nodemigrate-36239708922/`.

Migration run [36238216668](https://github.com/centerionware/not-k8s/actions/runs/36238216668)
used SHA `0254815476d1deefdb7c6465800675cceb51d1b4`. The five-node Docker
preflight and both standalone nodemigrate/branch-runtime builds passed. Both
source fixtures passed, then both lanes reached the nodestore target and passed
API CA, Node Ready, Cilium, DaemonSet, Deployment, Gateway, standalone Pod, and
CSI readiness PVC checks. The earlier generation failure is resolved at
runtime: the imported StatefulSet reported `generation=1` and
`observedGeneration=1`. Both lanes then failed the StatefulSet readiness wait.
At failure, `migration-stateful-0` was Pending and reported
`0/1 nodes are available: 1 node(s) had volume node affinity conflict`; its
PVC was reported Bound in the table but `kubectl describe pvc` showed Pending
with capacity `0`. Hostpath CSI pods were Running. This narrows the current
failure to migrated PV topology/binding or node scheduling; the exact PV
`nodeAffinity` and target Node label comparison was not captured, so component
ownership and root cause remain unconfirmed. The harness now emits the bound
PV YAML and target Node labels on failures. No semantic parity checkpoint,
return migration, or round trip passed. Logs: `/tmp/nodemigrate-36238216668/`.

Diagnostic run [36236810283](https://github.com/centerionware/not-k8s/actions/runs/36236810283)
used SHA `b4ae3a95b89abc037ffe3d27449aad72c4eedf40`. The five-node preflight
and both builds passed; both source lanes failed at the StatefulSet rollout
wait. New failure diagnostics showed `metadata.generation` and
`status.observedGeneration` were null. The root cause is create-on-apply in
`nodeapiserver` omitting server-assigned generation 1 for imported objects.
The fix and focused regression are now in the worktree; scoped quick-check
passed at [36238216478](https://github.com/centerionware/not-k8s/actions/runs/36238216478).
The migration rerun above confirms generation initialization and exposes the
next independent storage-topology failure. Logs: `/tmp/nodemigrate-36236810283/`.

Dedicated migration run [36235423620](https://github.com/centerionware/not-k8s/actions/runs/36235423620)
used SHA `d177f2c664e2b661da65d43275002bf79cf71b65`. Docker isolation and
both builds passed; focused nodeapiserver quick-check
[36235417698](https://github.com/centerionware/not-k8s/actions/runs/36235417698)
passed. In both K3s+Cilium and upstream Kubernetes+Cilium, migration reached
the destination API and passed the Gateway, Cilium, node, deployment,
standalone Pod, CA-trust, and CSI-readiness checks. Both lanes timed out at
StatefulSet rollout observation before semantic checkpoint, reverse
migration, or parity. Saved logs: `/tmp/nodemigrate-36235423620/`.
Generation/status and PVC/pod diagnostics have now been added to the harness;
their next run is pending. No standard full build or full e2e was dispatched.

## Latest release-backed result

Release-backed migration [36228920539](https://github.com/centerionware/not-k8s/actions/runs/36228920539)
used SHA `f86fad5622d48582c24531df4b88368096d632d9` and fetched regular
release `v0.8.0`. Both nodemigrate builds and the five-node Docker preflight
passed. K3s source fixture passed and captured 582 objects/59 CRDs. Import
failed on three Gateway API CRDs because v0.8.0's CEL runtime does not know
`matches`; the Gateway object could not be restored without its API. Target
snapshots showed no schedulable Nodes, no running Cilium or cert-manager Pods,
and no cert-manager webhook endpoints. The mounted-CA probe could not read a
Pod because `kubectl exec` reached nodelet on port 10250 and got connection
refused. The Node became Ready after rollback, so its later state is source
recovery evidence. Upstream also returned HTTP 500 importing CertificateRequest
and CSR objects and hit the same Gateway API CEL incompatibility. Rollback
restored both sources and retained protected exports. Neither lane reached
target workload/storage checks, reverse migration, or parity. This run does
not prove a mounted-CA mismatch. It points first to target Node readiness and
nodelet service availability; the watcher now records Node readiness on the
next run. Artifacts: `/tmp/nodemigrate-36228920539/`.

Release-backed migration [36228127861](https://github.com/centerionware/not-k8s/actions/runs/36228127861)
used SHA `d92abf28397e143c266c115cf2deb7d694f23374` and fetched the regular
release runtime `v0.8.0`. Both nodemigrate builds, the five-node Docker
preflight, and both source-stage CA checks passed. Both forward migrations
then failed. K3s Cilium/Traefik continued to reject the target API certificate
with `x509: certificate signed by unknown authority` even though namespace CA
ConfigMaps matched the target kubeconfig and nodemigrate seeded them before
workload import. The upstream lane returned HTTP 500 importing
`CertificateRequest/migration-test-1` and a CSR; it also rejected three current
Gateway API CRDs because this release's CEL runtime does not recognize
`matches`. Rollback restored both sources and retained protected exports. No
target workload/storage checkpoint, reverse migration, or parity check passed.
The mounted-CA fingerprint diagnostic was added after this run, so it remains
unverified. Focused nodemigrate crate tests passed at the same SHA in
[run 36228012548](https://github.com/centerionware/not-k8s/actions/runs/36228012548).
The general full build and full e2e gates were not dispatched.

## Latest branch-runtime result

The follow-up [run 36234240173](https://github.com/centerionware/not-k8s/actions/runs/36234240173)
used SHA `0a17cf9cb2e7d935afcb06e9653490aa76b1c912`. Focused
nodeapiserver quick-check [36234234075](https://github.com/centerionware/not-k8s/actions/runs/36234234075),
the Docker preflight, and both scoped builds passed. Both migrations again
reached destination API readiness, then timed out at GatewayClass Accepted.
The new diagnostic showed the cause: streaming-list initial ADDED events
assigned the collection snapshot revision to each object's `resourceVersion`.
Traefik submitted `1382` for K3s `migration-traefik` while the stored object's
mod_revision was `1049`; upstream submitted `891` while stored mod_revision
was `889`. The status handler correctly rejected these as stale/future
resourceVersions. The branch now constructs each initial event with its own
object mod_revision and leaves the snapshot revision on the completion
bookmark. A focused regression is added; nodeapiserver quick-check and the
runtime rerun for this fix are pending. Logs: `/tmp/nodemigrate-36234240173/`.

Dedicated migration run [36232994720](https://github.com/centerionware/not-k8s/actions/runs/36232994720)
used head SHA `56f27d13d05af278706f280f69c29f3d8d1bd195` with
`runtime_source=branch`. Focused nodemigrate checks passed separately in
[36232987555](https://github.com/centerionware/not-k8s/actions/runs/36232987555);
the five-node Docker preflight and both nodemigrate/combined-runtime builds
also passed. Both source fixtures passed and both forward migrations reported
destination API readiness. The canonical CRD resource query worked. In K3s,
target Node readiness, Cilium rollout, fixture DaemonSet, nginx Deployment,
namespace CA checks, and CSI readiness PVC binding passed. Both lanes then
timed out waiting for `GatewayClass/migration-traefik` Accepted. Traefik's
Gateway API controller repeatedly received HTTP 409 from destination
`/gatewayclasses/migration-traefik/status`, leaving its condition
`Accepted=Unknown` / `Waiting for controller`. This appears in both CNI lanes;
the precise stale-versus-current resourceVersion values were not logged in
that run. Follow-up diagnostics in run 36234240173 exposed the incorrect
snapshot revision described above. The sandbox-stop timeout did not recur.
No semantic checkpoint completed, and no return migration, parity comparison,
or merge gate passed. Full lane logs: `/tmp/nodemigrate-36232994720/`.

Dedicated migration run [36231779729](https://github.com/centerionware/not-k8s/actions/runs/36231779729)
used head SHA `df31d039178266efb513eb265781f10aa498eeb2`. Docker five-node
preflight and both nodemigrate/combined-runtime builds passed. In the K3s
lane, nodemigrate captured 582 objects and retained its protected export, but
`crictl stopp` on a source sandbox returned `DeadlineExceeded`; migration
aborted before destination API readiness, and source recovery passed. In the
upstream lane, nodemigrate captured 564 objects/55 CRDs, reported migration
complete, and the destination API passed readiness. Cilium and the migration
DaemonSet rolled out, then `verify_stage` stopped because the target API does
not advertise the kubectl `crd` shortcut (`the server doesn't have a resource
type "crd"`). The verifier now uses the canonical
`customresourcedefinitions.apiextensions.k8s.io` resource name. This was a
harness failure after forward migration, not a completed stage checkpoint;
reverse migration, parity, and the external-CNI API endpoint value remain
unverified. The branch now also tolerates a failed sandbox stop only when the
subsequent CRI container listing confirms that sandbox has no running
containers; focused nodemigrate CI and a runtime rerun are pending. Artifacts
were downloaded once to `/tmp/nodemigrate-36231779729/`.

Dedicated migration run [36229667964](https://github.com/centerionware/not-k8s/actions/runs/36229667964)
used head SHA `294a6c64`. The Docker five-node preflight and both standalone
utility/combined-runtime builds passed. K3s accepted all 59 CRDs and reached
destination API readiness. The target Node reported Ready; Cilium agent,
Envoy, and operator reached `1/1`. The live Cilium Pod's mounted
service-account CA fingerprint exactly matched the destination API CA
fingerprint. CoreDNS remained `Running` but `0/1 Ready`; nodelet journal output
shows ordinary Pod reconciliation paused behind the CoreDNS readiness gate,
and CoreDNS logs say its Kubernetes plugin was waiting for API synchronization
while the ready plugin remained unready. The logs do not expose the failed
request's underlying network or authorization error. Hostpath CSI setup failed.
Upstream accepted all 55 CRDs but failed
CertificateRequest import because the destination cert-manager webhook
Service had no endpoints. Both sources recovered and retained protected
exports. No workload/storage parity, reverse migration, or full round trip
passed. This proves branch-runtime K3s Cilium trust for the sampled live Pod;
it does not explain CoreDNS readiness or prove the full Cilium setup. Logs:
`/tmp/nodemigrate-36229667964/nodemigrate-k3s.log` and
`/tmp/nodemigrate-36229667964/nodemigrate-kubernetes.log`.

Follow-up source inspection found the K3s target's `default/kubernetes`
EndpointSlice still pointed at `127.0.0.1:6443` while its Node InternalIP was
`10.1.0.61`. Bootstrap refreshed this API endpoint only for Flannel, leaving
CoreDNS on external CNI without a Pod-reachable endpoint. The branch now
refreshes the nodeapiserver endpoint to the explicit advertise address or a
detected non-loopback host address for external CNI. Focused nodebootstrap CI
and the migration runtime rerun are pending; this is not yet runtime-verified.

Dedicated migration run [36226000144](https://github.com/centerionware/not-k8s/actions/runs/36226000144)
used SHA `edd697e94e009b7d3796de13b2ed68c076255625`. Both standalone utility
and combined-runtime builds passed, as did the five-node Docker preflight.
Both lanes passed `verify_stage source`, and repeated target snapshots showed
every namespace `kube-root-ca.crt` matching the destination admin kubeconfig
CA. Both then failed during forward import of
`CertificateRequest/migration-test-1` because cert-manager's webhook was
unreachable (HTTP 500). K3s Cilium and Traefik still logged
`x509: certificate signed by unknown authority` to the target API Service;
the ConfigMap check therefore does not prove the CA mounted inside already
started Pods is current. Rollback restored both source APIs and retained both
protected exports. No target workload/storage checkpoint, reverse migration,
or parity check passed. The current branch adds a namespace/CA readiness
barrier before importing workload controllers; this fix still needs focused
CI and a migration rerun. Logs are in `/tmp/nodemigrate-36226000144/`.

Dedicated migration run [36224872002](https://github.com/centerionware/not-k8s/actions/runs/36224872002)
used SHA `cf1b984a9427165401ec1fa8df386cc950539845`. Both utility and
combined-runtime builds and the five-node Docker preflight passed, but both
lanes stopped at `verify_stage source` before `nodemigrate` was invoked. The
new fixture assertion reported namespace CA mismatches because it decoded PEM
into shell command substitution, which strips trailing newlines. This run
provides no runtime evidence for the exporter change. The comparison now uses
the exact base64 CA bytes from kubeconfig and ConfigMap data; run 36226000144
later passed the source-stage check and repeatedly confirmed target ConfigMap
matches, while Cilium/Traefik trust and webhook failures remained. Focused nodemigrate tests passed
on the same SHA in [run 36224841930](https://github.com/centerionware/not-k8s/actions/runs/36224841930).
No general build or full e2e was dispatched.

Dedicated migration run [36223445443](https://github.com/centerionware/not-k8s/actions/runs/36223445443)
used SHA `9e8701291e2cd8f821f53e1e43d0bc306fd2f035`. The five-node Docker
preflight and both utility/combined-runtime builds passed. K3s reached target
API readiness, then hostPath CSI setup failed. Upstream import failed on
`CertificateRequest/migration-test-1` with HTTP 500; its source API recovered
and its protected export remained. Both lanes' nodeproxy journals show a
successful start and `service proxy watching Services + EndpointSlices`; the
inactive unit status was collected only after rollback stopped the service.
K3s Cilium and Traefik controllers repeatedly failed TLS verification to the
target API Service with `x509: certificate signed by unknown authority`. This
is consistent with stale source `kube-root-ca.crt` ConfigMaps being imported,
but the run did not record the namespace bundle contents. Neither lane reached
target workload/storage parity, reverse migration, or returned-source checks.

Logs: `/tmp/nodemigrate-36223445443/nodemigrate-k3s-36223445443/nodemigrate-k3s.log`
and `/tmp/nodemigrate-36223445443/nodemigrate-kubernetes-36223445443/nodemigrate-kubernetes.log`.
The manual dispatch skipped static validation. The follow-up change in this
branch adds CA-bundle snapshots, excludes the destination-managed root CA
ConfigMap from export, and checks every namespace bundle against the active API
CA; focused runtime verification is pending. No general build or full e2e was
dispatched.

Previous branch-runtime run [36222183166](https://github.com/centerionware/not-k8s/actions/runs/36222183166)
used runtime SHA `b093021677321124e8b8d318898819d90b71d927`. The five-node
preflight, utility/runtime builds, utility checks, and shell/snapshot checks
passed. Both lanes failed before target workload validation: K3s forward
migration/API readiness passed but hostPath CSI setup failed; upstream failed
the same CertificateRequest import and recovered its source API while
retaining the protected export. At least the early forward snapshots showed
nodeproxy inactive, but the artifact lacked its journal; that alone did not
establish a routing cause. Logs are under `/tmp/nodemigrate-36222183166/`.

Branch-runtime migration [run 36217850294](https://github.com/centerionware/not-k8s/actions/runs/36217850294)
used head SHA `d7b65846f55f24e75fd56ca54d107f5bad8b5511`. Both scoped utility
and combined-runtime builds passed in both lanes, as did the five-node Docker
preflight; both migration steps failed during forward object import. K3s
accepted the CiliumNode update with HTTP 200, so the earlier repeated 409 did
not recur. Import then failed applying `CertificateRequest/migration-test-1`
because the cert-manager admission webhook could not be reached. Failure-time
CoreDNS and Cilium pod diagnostics were collected after nodemigrate restored
the source services. They do not describe target state and cannot explain the
webhook failure. The new forward-migration watcher is intended to capture
target pods, logs, and cert-manager service endpoints before rollback; runtime
validation is pending. Both source services recovered and protected exports
were retained. Neither lane reached target workload/storage checks, reverse
migration, or parity. No general build gate or full e2e ran. Logs:
`/tmp/nodemigrate-36217850294-k3s.log` and
`/tmp/nodemigrate-36217850294-kubernetes.log`.

Previous branch-runtime migration [run 36216429427](https://github.com/centerionware/not-k8s/actions/runs/36216429427)
used head SHA `5a2a2630dc70ca27b5d0ed642a11319547f50869`. Both builds and the
Docker preflight passed. K3s reached target API readiness and the observed
CiliumNode update returned HTTP 200, but target hostpath CSI setup failed; all
three target CoreDNS pods were `Running` but `0/1` Ready, while most other
target pods were `Unknown` with no IP. That artifact had no CoreDNS details.
Upstream failed the same CertificateRequest webhook request with HTTP 500.
Both lanes restored the source and retained protected exports. Logs:
`/tmp/nodemigrate-36216429427-k3s.log` and
`/tmp/nodemigrate-36216429427-kubernetes.log`.

Branch-runtime migration [run 36214776875](https://github.com/centerionware/not-k8s/actions/runs/36214776875)
used runtime SHA `70d1a049` (workflow checkout included the later docs-only
commit `ef20bbc2`). Both scoped utility/runtime builds and the five-node Docker
preflight passed; both migration steps failed. In K3s, the nodelet CoreDNS
gate teardown fix remained effective: the replacement CSI Pods used target
`/var/lib/nodelet` paths. Cilium's agent started, but then exhausted ten
`CiliumNode/runnervmtr4k5` update retries with HTTP 409 / “object has been
modified” and exited fatally. The `agent-not-ready` taint remained, leaving
CSI Pods unschedulable; no target storage/workload, reverse-migration, or
parity checkpoint passed. No liveness-probe restarts were logged in this run,
but the agent did not reach stable Ready, so end-to-end verification of the
HTTP-probe fix remains pending. Upstream again failed importing
`CertificateRequest/migration-test-1` because the cert-manager webhook was
unreachable and returned HTTP 500. Rollback restored both source services and
retained their protected exports. The focused nodelet quick-check passed at
the same runtime SHA in [36214612947](https://github.com/centerionware/not-k8s/actions/runs/36214612947).
The general build gate and full e2e were not run. Logs:
`/tmp/nodemigrate-36214776875-k3s.log` and
`/tmp/nodemigrate-36214776875-kubernetes.log`.

The K3s failure artifact in run 36214776875 did not retain the API journal
lines from the CiliumNode conflict window: its fixed 1,000-line tail began
around 03:46:22Z, while Cilium failed at 03:44:16–03:44:26Z under heavy audit
traffic. The harness now captures matching CiliumNode API journal entries from
each migration's start. Run 36216429427 exercised this capture and observed a
successful HTTP 200 update, but did not reproduce the earlier conflict.

Branch-runtime migration [run 36212326763](https://github.com/centerionware/not-k8s/actions/runs/36212326763)
used SHA `b2c4f8824faf2c490aa13bf61f0a7c4509a27d05`. Both scoped utility and
runtime builds and the five-node Docker isolation preflight passed; both
`Run migration` steps failed. In K3s, the CoreDNS-gate teardown fix advanced
past the prior stale-CSI-Pod failure: new CSI Pods were created with
`/var/lib/nodelet` paths, but remained Pending because the node still had
`node.cilium.io/agent-not-ready:NoSchedule`. The Cilium agent container was
Running but not Ready; `cilium status` reported 42/44 controllers healthy,
85 modules OK, and 1/1 cluster nodes reachable. The operator had repeated
liveness-probe restarts, and its node-taint-sync controller started only
seconds before the migration timed out. These observations do not yet prove
which readiness condition or controller action is causal. Upstream again
failed applying `CertificateRequest/migration-test-1`: the cert-manager
webhook could not be reached and the API returned HTTP 500. Both lanes
restored the source and retained the protected export. Neither reached target
workload checks, reverse migration, or semantic parity. The regular build gate
and full e2e were not run. Logs:
`/tmp/nodemigrate-36212326763-k3s.log` and
`/tmp/nodemigrate-36212326763-kubernetes.log`.

## HTTP probe fix validation

The first validation of the HTTP probe fix used SHA `9c683295`. Focused
nodelet quick-check [36214053691](https://github.com/centerionware/not-k8s/actions/runs/36214053691)
and both branch-runtime builds in migration run
[36214053395](https://github.com/centerionware/not-k8s/actions/runs/36214053395)
failed to compile: the generated HTTP header field shape and a borrowed host
value were handled incorrectly. The Docker preflight in the migration run
passed; neither migration ran. A follow-up at SHA `bec61b9e` corrected those
types, but nodelet quick-check [36214473328](https://github.com/centerionware/not-k8s/actions/runs/36214473328)
found the header list is optional, not a nested vector. That was corrected at
SHA `70d1a049`; nodelet quick-check
[36214612947](https://github.com/centerionware/not-k8s/actions/runs/36214612947)
passed. Migration runtime validation at the same SHA completed in
[36214776875](https://github.com/centerionware/not-k8s/actions/runs/36214776875):
both builds and Docker preflight passed, but K3s then failed on repeated
CiliumNode 409 conflicts and upstream failed the cert-manager webhook request.
Neither migration path reached parity checks. General build and e2e gates
remain unrun.

Branch-runtime migration [run 36211105237](https://github.com/centerionware/not-k8s/actions/runs/36211105237)
used diagnostic SHA `0bc658e76b58f4f29ce2d69c310826fab8fe9ea9`. Both scoped
nodemigrate/runtime builds passed, as did the five-node Docker preflight; both
`Run migration` steps failed. K3s again completed all six Cilium init
containers but remained inside `wait_for_coredns()` through the hostpath CSI
timeout. At 02:35:39Z nodelet was still reconciling Cilium "while CoreDNS is
gated"; it had not accepted a normal Pod watch event for the terminating CSI
Pods. This confirms the gate left their teardown pending, rather than a CSI
installation failure. The gate now tears down local terminating Pods; its
focused nodelet check passed at fix SHA `f5efb03a` in
[36211663128](https://github.com/centerionware/not-k8s/actions/runs/36211663128),
but the runtime effect has not yet been tested. Upstream again failed to
restore `CertificateRequest/migration-test-1` because cert-manager's webhook
was unreachable; source rollback restored the API and retained the protected
export. Target workload, reverse-migration, and parity checks did not run in
either lane. The general build gate/full e2e were not run. Logs:
`/tmp/nodemigrate-36211105237-k3s.log` and
`/tmp/nodemigrate-36211105237-kubernetes.log`.

Branch-runtime migration [run 36195385046](https://github.com/centerionware/not-k8s/actions/runs/36195385046)
used SHA `6037e67e1f4e0a9d8d365d5ce8653c572c29ed72`. Both scoped runtime
builds and the five-node Docker isolation preflight passed. K3s source checks
passed and nodemigrate imported all 59 captured CRDs; destination API
readiness passed. Cilium then stopped at `mount-bpf-fs`: the CRI stop event
included Pod metadata, CRI later reported exit 0, but the captured Pod status
still said Running and no following init container was created. CSI setup
timed out. Upstream accepted all 55 captured CRDs, then CertificateRequest
admission returned HTTP 500 after destination Cilium networking disappeared.
Both lanes restored the source and retained protected exports; neither reached
target workloads, reverse migration, or semantic parity. The CiliumNode 409
diagnostic was not exercised in this run. Logs are at
`/tmp/nodemigrate-36195385046/{k3s,kubernetes}/`.

Focused `nodeapiserver` quick-check [36195027134](https://github.com/centerionware/not-k8s/actions/runs/36195027134)
passed on the same SHA. Branch-runtime rerun [36197970992](https://github.com/centerionware/not-k8s/actions/runs/36197970992)
completed at SHA `9974e76329349d33205aa53c824e3b2fb864f6e7`: both scoped builds
and Docker preflight passed, but both `Run migration` steps failed (K3s after
21m18s, upstream after 16m03s). GitHub job metadata confirms those step
failures. Captured K3s diagnostics show Cilium remained at `Init:3/6` after
`mount-bpf-fs` logged a successful bpffs mount but CRI had no live task; a
later CoreDNS sandbox setup failed when `cilium-cni` returned `signal:
killed`. Both lanes also had a confirmed ResourceQuota status-write loop from
comparing unsupported `requests.storage` usage against the controller's
partial `pods`/`services` map. The fix is in progress; no destination
workload, reverse migration, or parity checkpoint passed. Logs:
`/tmp/nodemigrate-361979-k3s.log` and
`/tmp/nodemigrate-361979-kubernetes.log`. Focused `nodelet` quick-check
[36197970777](https://github.com/centerionware/not-k8s/actions/runs/36197970777)
passed on SHA `9974e763`.

The endpoint/Lease preservation fix was tested at SHA
`d4f3c4a0479abd3f3e1d6e416bec2b2b2d12cf61`: focused nodemigrate checks
[36199446770](https://github.com/centerionware/not-k8s/actions/runs/36199446770)
and migration-workflow static validation
[36199446771](https://github.com/centerionware/not-k8s/actions/runs/36199446771)
passed. No runtime migration has yet exercised those added fixture resources.

Branch-runtime migration [run 36192756836](https://github.com/centerionware/not-k8s/actions/runs/36192756836)
used SHA `c70f53023a4a48c04b0d7b85d4dda3b6388a2b50`. Nodemigrate and
combined-runtime builds passed in both lanes, and the five-node Docker
isolation preflight passed. K3s source checks passed; the utility captured 582
objects/59 CRDs, imported all CRDs, and reached destination API readiness.
Destination Cilium then exhausted ten `CiliumNode` update retries with HTTP
409, preventing the target workload/CSI checkpoint, reverse migration, and
parity comparison. The new API-server conflict diagnostics did not appear in
this run; a status-subresource stale-version diagnostic has now been added for
the next iteration. Upstream source checks passed and the utility captured
562 objects/55 CRDs, but CertificateRequest admission returned HTTP 500 after
the Cilium CNI plugin/agent disappeared; rollback restored the source and
retained the protected export. Neither lane passed target, reverse, or parity
checks. Logs are at `/tmp/nodemigrate-36192756836/{k3s,kubernetes}/`.

The newest migration [run 36189354168](https://github.com/centerionware/not-k8s/actions/runs/36189354168)
used SHA `7da932bf9f15c943a8aa025739b5e6706bc95386`. Focused `nodelet`
quick-check [36189351742](https://github.com/centerionware/not-k8s/actions/runs/36189351742),
the five-node Docker isolation preflight, and both nodemigrate/combined-runtime
builds passed. K3s Cilium init containers progressed to starting the agent,
which then exhausted ten retries updating `CiliumNode/runnervmtr4k5` with
HTTP 409 conflicts. Upstream imported 55 CRDs, then CertificateRequest
admission failed because the cert-manager webhook was unreachable while CNI
was down. Rollback and protected-export retention passed in both lanes. Neither
lane reached target workload checks, reverse migration, or parity. The logged
Cilium CRI events had Pod metadata; the missing-metadata fallback remains
unproven. Full logs: `/tmp/nodemigrate-36189354168/`. The branch now adds
nodeapiserver diagnostics to distinguish stale resourceVersion conflicts from
storage compare-and-swap races; focused nodeapiserver and migration reruns are
pending.

The newest branch-runtime migration [run 36186694756](https://github.com/centerionware/not-k8s/actions/runs/36186694756)
used SHA `d4be86e33d9140faa9258eaff112bebed3216fa4`. The nodemigrate and
combined-runtime builds and five-node Docker isolation preflight passed. The
focused `nodelet` quick-check passed on the same SHA in
[run 36186694438](https://github.com/centerionware/not-k8s/actions/runs/36186694438).
K3s source setup/import passed (59 CRDs accepted, API ready), but the target
did not become CNI-ready: `config`, `mount-cgroup`, and
`apply-sysctl-overwrites` completed; `mount-bpf-fs` logged a successful bpffs
mount and later had no live containerd task, while the captured Pod status
still said Running. Upstream imported 55 CRDs and then failed
CertificateRequest admission because its webhook could not be reached while
CNI was unavailable. Rollback and protected-export retention passed in both
lanes. Neither lane reached target workload checks, reverse migration, or
parity. Logs were saved at `/tmp/nodemigrate-36186694756/`. The nodelet event
fallback has passed its component tests but has not yet been confirmed to
resolve this final init-status transition; the next run needs to expose event
arrival, Pod lookup, and reconcile outcome.

Branch-runtime migration [run 36176504323](https://github.com/centerionware/not-k8s/actions/runs/36176504323)
used code SHA `77e023f1349a6345f5a2bccd1e3a838887b3bbc5`. Both nodemigrate and
combined-runtime builds passed, as did the five-node Docker isolation
preflight, but neither migration completed. K3s imported 58 CRDs and reached
destination API readiness. Its Cilium `mount-bpf-fs` init container printed
that bpffs was mounted, then remained CRI-Running for over a minute; later
init containers did not start, Cilium kept its not-ready taint, and workload
and CSI checks could not pass. The runtime evidence confirms the container
was still reported running; the underlying cause is not yet established.
Upstream imported all 54 CRDs, but CertificateRequest admission returned HTTP
500 while the cert-manager webhook was unavailable after destination Cilium
failed to initialize. Rollback restored the source in both lanes and retained
the protected exports. Neither lane reached a target checkpoint, reverse
migration, or parity comparison. Full logs:
`/tmp/nodemigrate-36176504323/{k3s,kubernetes}.log`.

The current PR head `7292bb8880cb1096c6fce195a4bbc9c9368d2ac1` passed
nodemigrate tests, focused checks/packaging, release policy validation, and
commit convention checks in [run 36177265014](https://github.com/centerionware/not-k8s/actions/runs/36177265014),
[run 36177264886](https://github.com/centerionware/not-k8s/actions/runs/36177264886),
[run 36177264782](https://github.com/centerionware/not-k8s/actions/runs/36177264782),
and [run 36177260453](https://github.com/centerionware/not-k8s/actions/runs/36177260453).
Those checks do not exercise the newer immutable ConfigMap, multi-version CRD,
or allowed/denied NetworkPolicy fixtures at runtime; a current-head migration
run is still required.

Current-head branch-runtime migration [run 36179275793](https://github.com/centerionware/not-k8s/actions/runs/36179275793)
used code SHA `7292bb8880cb1096c6fce195a4bbc9c9368d2ac1`. Nodemigrate and
combined-runtime builds passed in both lanes, as did the five-node Docker
isolation preflight. Both lanes reached the expanded source fixtures; the
allowed NetworkPolicy Job completed successfully, but a redundant follow-up
log assertion failed because its `grep -q` command suppresses output. This is
a harness defect fixed in the current worktree. The denied-policy probe and
all migration stages were skipped; no migration parity was tested. Logs:
`/tmp/nodemigrate-36179275793/nodemigrate-{k3s,kubernetes}-36179275793/`.

The previous focused nodemigrate quick-check passed at SHA
`29b6b7de9575e7cf0d43ca60becba868373d86f3` in
[run 36168559894](https://github.com/centerionware/not-k8s/actions/runs/36168559894).
It does not include the new CNI-path fix.

## Workflow

- `.github/workflows/nodemigrate.yml` runs the focused crate tests and release
  policy check. It uploads the complete crate-test log for seven days and adds
  compiler/test-failure context (up to 3000 characters) to both a failure
  annotation and the step summary.
- `.github/workflows/nodemigrate-integration.yml` runs shell syntax validation
  on relevant pull requests.
- Manual `workflow_dispatch` starts isolated K3s and upstream Kubernetes
  lanes. Each lane builds the PR's standalone `nodemigrate` utility, downloads
  the combined `notk8s` runtime from the latest regular release, verifies the
  GitHub-published SHA-256 digest and required components, then runs
  `.github/scripts/nodemigrate-integration.sh` as root and uploads its log.
  Set `docker_only=true` to run the five-node Docker capability/isolation
  preflight without repeating the K3s and upstream lanes. Set
  `runtime_source=branch` to build the combined runtime from the tested branch
  for the single-node lanes; the default `release` mode remains the v0.8.0
  baseline check. Neither option runs the general build/e2e workflows.
  The latest regular release was verified as `v0.8.0` on 2026-09-24, so the
  PR utility runs against that release. Run `36065059567` completed the
  utility builds, runtime downloads, full source CNI/Cilium/CSI/workload
  checks, and target bootstrap in both lanes. K3s forward migration completed
  with 48 CRDs accepted. Run `36066311951` verified that the fixture now
  leaves those CRDs intact and can read the restored VolumeSnapshotClass, but
  the v0.8.0 target returned 403 when `system:kube-controller-manager` tried
  to create CSI pods. Upstream import still reaches two object failures: CSR
  ExtraValue encoding and the target's missing ClusterTrustBundle/v1 API. The
  Docker image builds, but its five-node systemd probe still exits 255.
  The kubeadm source setup installs `crictl` from the matching cri-tools minor
  release (override with `CRI_TOOLS_VERSION`) and records the resolved tool
  version for static-pod cleanup diagnostics.
- Branch-runtime migration run `36078155500` built the combined runtime from
  the PR source and proved the remaining 403 was in server-side authentication:
  K3s and kubeadm requests to create workload ReplicaSets were audited as
  `system:kube-controller-manager`, despite nodecontroller sending per-controller
  impersonation headers. The API server did not process those headers. An RBAC-
  checked impersonation implementation and focused parser tests are now in the
  worktree. Quick-check run `36080298252` first found a missing mutable borrow;
  after the one-line compile fix, `nodeapiserver,nodemigrate` passed in run
  `36080595243` on SHA `d4847449a84a5014515f31b3d9d522e455910cf7`. Branch-runtime
  rerun `36082829012` built nodemigrate and the combined branch runtime in both
  lanes, and the five-node Docker preflight passed. Both migrations reached
  destination API readiness but failed post-cutover with the CoreDNS gate
  blocking Cilium. Nodelet now reconciles local host-network Pods during the
  gate; focused `nodelet` quick-check passed in `36084558060`. Branch-runtime
  migration rerun `36084558323` confirms Cilium sandbox/init startup now runs,
  but remains blocked on runc's read-only-root mountpoint failure for
  `/var/run/cilium/envoy/sockets`; CNI and CoreDNS stay unready, so CSI stays
  `Terminating`. The next fix targets nodelet CRI volume mounts under a
  read-only image. Diagnostics now save nodelet journal, CRI sandboxes and
  containers, Cilium Pod YAML, and events.
- The next branch-runtime run, [36095002578](https://github.com/centerionware/not-k8s/actions/runs/36095002578),
  used SHA `3e9c0940bb4331eca01a83b5bb89f12f0f615585`. Utility/runtime builds
  and five-node Docker preflight passed; both migration lanes completed
  forward migration, then failed Cilium/CSI readiness. Captured Envoy logs
  showed `errno=98` binding `/var/run/cilium/envoy/sockets`. CRI inspection
  found the source Envoy sandbox still running beside the migrated Envoy Pod.
  The worktree now stops/removes source CRI sandboxes before destination
  activation, with source-service rollback on cleanup failure. Focused
  nodemigrate quick-check and migration rerun are pending; no round trip or
  parity gate has passed.
- On fix SHA `8d4f4a690f906a7fe4de2cbb88778b27599029cb`, focused
  `nodemigrate` quick-check passed in [run 36096815873](https://github.com/centerionware/not-k8s/actions/runs/36096815873).
  The migration run [36096822314](https://github.com/centerionware/not-k8s/actions/runs/36096822314)
  passed both nodemigrate/runtime builds and Docker preflight, but neither
  migration lane passed: upstream rollback handled a containerd `rmp`
  `DeadlineExceeded` while removing a stopped sandbox; K3s advanced through
  cutover but Envoy still hit `errno=98` on a stale Cilium socket. The
  worktree now orders Cilium teardown last, checks that no source containers
  are running before tolerating stale sandbox metadata, and cleans only Unix
  sockets in the detected Cilium Envoy directory. Focused crate verification
  and migration rerun are pending.
- On SHA `4e41430e2f1b7805b36ad9562f8423ec7f4e1a54`, `nodemigrate`
  quick-check passed in [run 36098539568](https://github.com/centerionware/not-k8s/actions/runs/36098539568),
  and utility/runtime builds plus the Docker preflight passed in migration
  [run 36098546161](https://github.com/centerionware/not-k8s/actions/runs/36098546161).
  K3s completed forward migration but Cilium Envoy still failed with
  `errno=98`; cleaning filesystem Unix sockets did not resolve the collision.
  The upstream lane completed bootstrap but import failed on
  `cert-manager.io/v1/CertificateRequest migration-test-1`, then restored the
  source API and retained the export. Its log omitted the underlying API error.
  The worktree now preserves complete import error chains and adds host socket,
  directory, and process diagnostics for the next Cilium failure. No round
  trip, reverse migration, or parity gate passed.
- At SHA `fa9d5ae34c77772d9a020a0251b706943f8b1cd7`, the targeted
  `nodemigrate` quick-check passed in [run 36100274964](https://github.com/centerionware/not-k8s/actions/runs/36100274964).
  Branch-runtime integration [run 36100281843](https://github.com/centerionware/not-k8s/actions/runs/36100281843)
  passed both utility/runtime builds and the five-node Docker preflight, then
  failed both migration lanes. K3s again failed Cilium readiness with
  `errno=98`; `ss` captured active agent/Envoy socket owners and `ps` showed
  Envoy under a containerd shim, but the diagnostics did not map that shim to
  its CRI sandbox. The code review found K3s cleanup defaulted to the generic
  `/run/containerd/containerd.sock`; the worktree now detects K3s's configured
  endpoint and defaults to `/run/k3s/containerd/containerd.sock`. Upstream
  bootstrap succeeded, then applying
  `cert-manager.io/v1/CertificateRequest migration-test-1` returned HTTP 500.
  Full error reporting exposed the object and route; the destination API
  server cause is still unknown. The source API recovered and the protected
  export remained available. No reverse migration or semantic parity check
  ran. The endpoint change is pending runtime retest;
  the CertificateRequest failure is pending nodeapiserver diagnosis and fix.
- The focused rerun for endpoint detection, quick-check [36102292310](https://github.com/centerionware/not-k8s/actions/runs/36102292310)
  on SHA `e9ed0b48144987d1b36629eed372b23d96686659` failed at compilation:
  the new unit test referenced `K3sConfig` without importing it into the test
  module. After adding that import, the focused `nodemigrate` quick-check
  passed on SHA `f5a2ccb6f35bc72237d904c7c75bacda63a26fac` in
  [run 36102588479](https://github.com/centerionware/not-k8s/actions/runs/36102588479).
  Branch-runtime migration [36102292369](https://github.com/centerionware/not-k8s/actions/runs/36102292369)
  on SHA `e9ed0b48144987d1b36629eed372b23d96686659` passed both utility/runtime
  builds and the Docker preflight, but both migration lanes failed. K3s CRI
  cleanup ran after stopping K3s, so the embedded socket at
  `/run/k3s/containerd/containerd.sock` refused the connection. Upstream
  restored the source API after a CertificateRequest HTTP 500; its branch
  runtime did not report a CSR ExtraValue error.
- Latest-release migration [36102899864](https://github.com/centerionware/not-k8s/actions/runs/36102899864)
  on SHA `d23ac3b606126865c11f412a2712fac7a88a9bea` built the current utility,
  downloaded the latest regular release `v0.8.0`, and passed the Docker
  preflight. Both migration lanes failed. The K3s lane confirmed the same
  embedded CRI shutdown ordering. The upstream lane's dedicated API journal
  showed the CertificateRequest 500 came from the unavailable
  `cert-manager-webhook` service while target Cilium networking was unhealthy;
  containerd reported the Cilium CNI plugin exited with `signal: killed`.
  The known `v0.8.0` CSR `ExtraValue` protobuf defect also reproduced, while
  the branch-runtime lane did not report that failure. Source rollback and
  protected export retention passed. No reverse migration or parity gate ran.
  The worktree now moves K3s sandbox cleanup ahead of service shutdown; that
  change and the target Cilium/webhook path require focused and runtime retest.
- At SHA `459570ee4fdcea8128456f84058882159111a85a`, focused nodemigrate
  quick-check passed in [run 36104107511](https://github.com/centerionware/not-k8s/actions/runs/36104107511).
  Branch-runtime migration [run 36104132045](https://github.com/centerionware/not-k8s/actions/runs/36104132045)
  passed the nodemigrate/runtime builds and Docker preflight. K3s imported all
  48 discovered CRDs and passed destination API readiness, then failed the
  post-cutover CSI fixture because Cilium/CNI remained unready
  (`cni plugin not initialized`); CoreDNS and hostPath CSI Pods stayed
  unready, so no return migration ran. Captured Envoy output shows the target
  Envoy container exiting with `errno=98` while a separate Envoy process still
  owns the host sockets; the containerd-shim command line/cgroup was not
  captured, so its source owner is not yet proven. The upstream target reached
  import but failed applying `cert-manager.io/v1/CertificateRequest migration-test-1`
  with HTTP 500 because the cert-manager webhook URL was unreachable. The
  source API recovered and the export remained available. Cilium/CNI readiness
  remains unresolved.
- Latest-release migration [run 36104248971](https://github.com/centerionware/not-k8s/actions/runs/36104248971)
  at the same utility SHA verified and used the regular `v0.8.0` runtime.
  K3s also passed forward import and destination API readiness, but then hit
  the same Cilium/CNI and CSI readiness failure. The upstream lane reproduced
  the unreachable cert-manager webhook and the known `v0.8.0` CSR `ExtraValue`
  protobuf failure; rollback restored the source API and retained the export.
  Neither source lane reached a round trip or full parity checkpoint.
- The migration CLI warns about the high data-loss risk and requires exact
  `yes` on an interactive terminal. Noninteractive runs write the same
  `⚠️⚠️⚠️⚠️⚠️` warning to stderr for service and CI logs, naming the high
  probability of data loss, external backups, and user responsibility, then
  continue without prompting. Focused crate tests cover exact interactive
  confirmation and noninteractive warning behavior.
- At SHA `954f1be3d12fb7f0334db8dff6efae28ca0e4250`, object import now falls
  back to a destination API version only when discovery confirms the same API
  group and kind, and logs the selected conversion. Focused nodemigrate tests
  passed in [run 36068373192](https://github.com/centerionware/not-k8s/actions/runs/36068373192).
  The authorized release-backed migration against latest regular release
  `v0.8.0` completed unsuccessfully in [run 36068417485](https://github.com/centerionware/not-k8s/actions/runs/36068417485).
  Both lanes built the utility and downloaded the release runtime. K3s again
  reached migration but hit the v0.8.0 controller-manager 403 during CSI
  readiness. Upstream version negotiation successfully applied the source
  ClusterTrustBundle through `certificates.k8s.io/v1beta1`; the only failed
  object was a CSR, for which the v0.8.0 server returned HTTP 500 while
  decoding `certificates.k8s.io/v1.ExtraValue`. The PR already contains a
  nodeapiserver codec fix, but that code is not present in the release runtime.
  The five-node image built, but its systemd preflight exited 255 before
  readiness.
- Manual dispatch also starts the Docker five-node preflight. The Dockerfile
  path fix worked, but the next image build found Ubuntu 24.04 does not offer
  `bpftool` as an installable package name; the follow-up uses its providing
  kernel tools package. It builds
  `.github/nodemigrate/five-node.Dockerfile`, then
  `.github/scripts/nodemigrate-docker-preflight.sh` starts five disposable
  systemd containers and checks namespace separation, containerd CRI, a loaded
  eBPF classifier, per-node volumes, inter-node reachability, and single-node
  failure isolation. It does not yet provision Kubernetes or test Cilium
  datapath or migration parity.
- The script installs the selected upstream distribution first, uses Cilium
  with Flannel disabled in the K3s lane, installs the hostPath CSI test
  driver, cert-manager, Traefik, nginx, static and CSI-backed claims, then
  checks the source → nodestore → retained-source round trip. Every checkpoint
  asserts the Cilium DaemonSet rollout and CiliumEndpoint CRD are present.
- Every checkpoint writes a private canonical snapshot of important behavior
  and a fingerprint inventory for every listable API object nodemigrate is
  expected to transfer. Fingerprints use the export sanitizer's same treatment
  of status, API-assigned identity fields, controller-regenerated objects, and
  service-account token Secrets. Standalone Pods and ReplicaSet/ControllerRevision
  rollout history are included; live NodeMetrics and PodMetrics samples are omitted. Secret
  content is hashed in memory and never written to checkpoints. The script
  compares source→nodestore object fingerprints and requires the returned
  snapshot to match the source.
- The fixture includes a ConfigMap with `nodemigrate.io/source-uid`, plus a
  Node with a custom label, annotation, and `PreferNoSchedule` taint. Every
  checkpoint checks that the Node metadata survives; the round-trip snapshot
  includes it. ConfigMap data is hashed in the private checkpoint and compared
  on return.
  Runtime results are still required to demonstrate state parity.
- Runtime coverage for joining an existing cluster and replacing a member is
  a separate required scenario; the current script does not establish that
  case as verified.

## Required merge gates

Both isolated round trips are mandatory nodemigrate merge gates. Neither has
passed, so nodemigrate must not be merged until the evidence is recorded here.
Both gates must exercise the complete [required migration test inventory](NODEMIGRATE_MIGRATION_STATUS.md#required-migration-test-inventory),
including ConfigMaps, CRDs and custom resources, Deployments, StatefulSets,
Helm chart releases, Jobs and CronJobs, DaemonSets, Ingress and Gateway API,
RBAC, storage, networking, admission, and every other listable API kind
reported by source discovery. Each applicable resource must migrate and pass
identity, durable-state, and behavior checks at the source, not-k8s, and
returned-source checkpoints. Classify any regenerated or lifecycle-excluded
kind with its reason; no resource group is waived because it is not in the
current fixture.

| Gate | Required topology and round trip | Pass criteria | State |
| --- | --- | --- | --- |
| K3s single node | One-node K3s with Cilium → not-k8s → retained K3s | At initial source, after migration to not-k8s, and after return: verify node identity/readiness, workloads, add-ons/custom resources, ingress, certificates, persistent-volume bindings and data, and Cilium. The returned full migration-state checkpoint has no semantic differences from the initial checkpoint. | Not run |
| Upstream five nodes | kubeadm Kubernetes with three control-plane nodes and two workers → not-k8s → retained upstream Kubernetes | At every stage, verify all five node identities, roles, membership, and readiness plus the same workload, add-on/custom-resource, ingress, certificate, persistent-volume/data, and Cilium checks. Exercise existing-cluster join/replacement. The returned full migration-state checkpoint has no semantic differences from the initial checkpoint. | Not run |

Checkpoint comparison normalizes API-assigned UIDs and regenerated status;
controller/transient objects and live NodeMetrics/PodMetrics samples are not
persistent migration state. Compare all other exported API state and verify
live behavior independently. A skipped checkpoint, missing probe, or
unexplained difference is not a pass.

The five-node lane must run with five distinct isolated nodes, which may share
one CI host using QEMU or another suitable isolation mechanism. GitHub documents
nested virtualization on hosted runners as technically possible but
experimental and unsupported, so the workflow must not assume QEMU/KVM is
available on a standard hosted runner ([GitHub-hosted runner guidance](https://docs.github.com/en/actions/concepts/runners/github-hosted-runners)).
The existing integration workflow uses `ubuntu-latest`, and the GitHub API
reported no self-hosted runners registered for this repository when the
five-node lane was reviewed. Docker is therefore the initial candidate only
if a capability preflight demonstrates
five distinct node identities and filesystems, systemd/service control, CRI,
network namespaces, Cilium/eBPF behavior, per-node storage, and node
failure/isolation behavior required by these checks. A container-only result
does not verify behaviors it does not model. If the preflight fails, the lane
needs a runner with a guaranteed suitable virtualization environment. Record
the isolation method, topology, demonstrated capabilities, and any unmodeled
behavior with each run. The current integration workflow does not yet
provision five distinct nodes or pass either full merge gate.

## Multi-node implementation gap

The present script runs source, not-k8s, and return stages on one host OS. It
does not provision five independently isolated nodes. The migration utility
now exposes an operator-ordered reverse control-plane protocol: `stage-target`
starts a retained CP without waiting for etcd quorum; after retained-cluster
quorum returns, `import-export=/path` loads the protected export and imports
cluster API state; later nodes can use `skip-api-export` after that import.
The utility does not automatically sequence nodes or prove that prior stages
completed beyond checking destination API readiness where required. No real
three-CP run verifies this protocol yet. Repeating the current single-host
script or running it once per node would not satisfy the five-node gate.

The integration script now supports source-library mode for a future
coordinator, optional exact node and control-plane/worker-count checks, an
optional hostname affinity for the static hostPath PV, and a Cilium API
endpoint override. These hooks are not yet wired to five Docker nodes and do
not count as multi-node coverage.

The Docker capability preflight and systemd node image are implemented but
not run. Next, use them to provision kubeadm with three control planes and two
workers inside those nodes, install and validate Cilium and the workload
fixtures, then add an orchestrated staged control-plane cutover/return lane.
The existing probe is only an infrastructure check; it is not evidence of
Kubernetes, Cilium, storage-controller, or nodemigrate behavior. Use QEMU only
on a runner whose virtualization capability is demonstrated; hosted runner
support is not guaranteed.

## Run policy

- General `build.yml`: excluded by the user for this task.
- General e2e workflow: excluded by the user for this task.
- Local Cargo build/test/e2e: prohibited by repository host constraints.
- Targeted `nodemigrate checks`: the focused compile and test check for
  nodemigrate Rust changes. `quick-check.yml` does not currently include the
  nodemigrate crate; use `components=nodebootstrap` when nodebootstrap changes
  and `components=nodestore` when Raft membership code changes. Run the
  applicable focused checks for every changed crate; do not run unrelated
  crates.
- Dedicated migration workflow: migration runtime tests are authorized by the
  user for this task. It builds only `nodemigrate` as test setup and uses the
  latest regular `notk8s` release runtime; this is not the general build or
  e2e gate. Do not dispatch general `build.yml` or e2e workflows.
- Static shell syntax, formatting, diff whitespace, and docs-link checks are
  allowed and must be reported separately from runtime evidence.

## Verification log

The latest branch-runtime run used SHA `f048ae142d8dbfe6ee9fa28e6a7a2a8c4a51702c`
in [run 36093516629](https://github.com/centerionware/not-k8s/actions/runs/36093516629).
Nodemigrate and combined-runtime builds passed, nodelet quick-check passed in
[run 36093508613](https://github.com/centerionware/not-k8s/actions/runs/36093508613),
and the five-node Docker isolation preflight passed. Both migration lanes
completed forward migration. Neither reproduced the old runc mountpoint error,
but Cilium Envoy failed its startup probe and CNI/CoreDNS stayed unready, so
post-cutover workload/storage checks, reverse migration, and parity comparison
did not run. `crictl inspect` captured the final OCI spec; the next diagnostic
run also captures Envoy container logs. Full logs are saved at
`/tmp/nodemigrate-36093516629/`. No local Cargo test/build was run.

The prior run `36089689047` reproduced the mount failure without OCI inspection;
its logs are at `/tmp/nodemigrate-36089689047/artifacts/`.

Earlier release-backed run used SHA
`093f2e440b16c4a2a585b424b816b97ab49480a9`. Focused nodemigrate crate tests
and packaging, integration shell validation, and commit convention passed
(runs `36058334362`, `36058334455`, and `36058331222`). Manual
release-backed run `36058570338` built nodemigrate and verified the `v0.8.0`
digest/components. Both sources passed workload/storage setup and emitted the
unattended `⚠️⚠️⚠️⚠️⚠️` data-loss warning. Export captured 506 objects including
48 CRDs from K3s, and 488 objects including 44 CRDs upstream. Both targets
bootstrapped. The importer waited 60 seconds, but destination discovery still
omitted all 51 K3s and 47 upstream served source CRD APIs; import failed with 37
and 26 objects pending. K3s also reported its Addon API; upstream reported
CertificateSigningRequest and ClusterTrustBundle failures. The run does not
establish whether each CRD write persisted or why destination discovery omitted
the APIs, and it does not verify a round trip. The five-node Docker image
built, but its first systemd container exited 255 before readiness. See
[run 36058570338](https://github.com/centerionware/not-k8s/actions/runs/36058570338).
No local Cargo test/build was run.

| Date | SHA | Check/lane | Result | Evidence |
| --- | --- | --- | --- | --- |
| 2026-09-26 | `0bc658e7` | Branch-runtime K3s + Cilium and upstream + Cilium | Scoped nodemigrate/runtime builds and five-node Docker preflight passed. K3s completed all six Cilium init containers but its nodelet remained inside the CoreDNS gate and left the CSI Pods terminating until hostpath setup failed. Upstream CertificateRequest restore again failed at webhook admission; rollback restored source and retained export. Neither lane reached workload, reverse, or parity checks. | [Run 36211105237](https://github.com/centerionware/not-k8s/actions/runs/36211105237); logs `/tmp/nodemigrate-36211105237-{k3s,kubernetes}.log` |
| 2026-09-26 | `f5efb03a` | Targeted quick-check: `nodelet` | Passed unit tests, including the new startup-gate predicate regression for local and remote terminating Pods. | [Run 36211663128](https://github.com/centerionware/not-k8s/actions/runs/36211663128) |

| 2026-09-25 | `f048ae142d8dbfe6ee9fa28e6a7a2a8c4a51702c` | Targeted quick-check: `nodelet` | Passed unit tests for nested read-only managed-volume mountpoint preparation and external-volume non-mutation. | [Run 36093508613](https://github.com/centerionware/not-k8s/actions/runs/36093508613) |
| 2026-09-25 | `f048ae142d8dbfe6ee9fa28e6a7a2a8c4a51702c` | Branch-runtime K3s + Cilium | Forward migration passed; prior runc mountpoint failure did not recur. Cilium Envoy still failed its startup probe and blocked CNI/workload/storage checks. | [Run 36093516629](https://github.com/centerionware/not-k8s/actions/runs/36093516629) |
| 2026-09-25 | `f048ae142d8dbfe6ee9fa28e6a7a2a8c4a51702c` | Branch-runtime Kubernetes + Cilium | Forward migration passed; old mountpoint failure did not recur, but Envoy startup probe failed and blocked CNI/workload/storage checks and reverse migration. | [Run 36093516629](https://github.com/centerionware/not-k8s/actions/runs/36093516629) |
| 2026-09-25 | `f048ae142d8dbfe6ee9fa28e6a7a2a8c4a51702c` | Five-node Docker preflight | Passed; validates isolation/capabilities only, not Kubernetes/Cilium/migration parity. | [Run 36093516629](https://github.com/centerionware/not-k8s/actions/runs/36093516629) |
| 2026-09-25 | `2debf9caf19def33d1253e046f85be8e1063d2d9` | Branch-runtime K3s + Cilium | Utility/runtime builds and forward migration passed; Envoy failed because writable nested targets were absent from a nodelet-managed read-only parent volume. OCI spec captured. | [Run 36091505933](https://github.com/centerionware/not-k8s/actions/runs/36091505933) |
| 2026-09-25 | `2debf9caf19def33d1253e046f85be8e1063d2d9` | Branch-runtime Kubernetes + Cilium | Forward migration passed; the same nested read-only-parent mount failure blocked CNI, storage/workload verification, and reverse migration. OCI spec captured. | [Run 36091505933](https://github.com/centerionware/not-k8s/actions/runs/36091505933) |
| 2026-09-25 | `2debf9caf19def33d1253e046f85be8e1063d2d9` | Five-node Docker preflight | Passed; validates isolation/capabilities only, not Kubernetes/Cilium/migration parity. | [Run 36091505933](https://github.com/centerionware/not-k8s/actions/runs/36091505933) |
| 2026-09-25 | `e229ae742ab64327512086decdb9a08d58dbfe81` | Branch-runtime K3s + Cilium | Source setup and forward migration passed; the read-only-root Envoy mountpoint error remained after the parent-first mount-order change. CNI/CoreDNS, storage/workload checks, reverse migration, and state comparison did not run. | [Run 36089689047](https://github.com/centerionware/not-k8s/actions/runs/36089689047) |
| 2026-09-25 | `e229ae742ab64327512086decdb9a08d58dbfe81` | Branch-runtime Kubernetes + Cilium | Forward migration passed; the identical Envoy mountpoint failure blocked post-cutover checks and reverse migration. | [Run 36089689047](https://github.com/centerionware/not-k8s/actions/runs/36089689047) |
| 2026-09-25 | `e229ae742ab64327512086decdb9a08d58dbfe81` | Five-node Docker preflight | Passed; validates isolation/capabilities only, not Kubernetes/Cilium/migration parity. | [Run 36089689047](https://github.com/centerionware/not-k8s/actions/runs/36089689047) |
| 2026-09-25 | `e229ae742ab64327512086decdb9a08d58dbfe81` | Targeted quick-check: `nodelet` | Passed focused CRI mount tests, but the mount-order change did not fix the Envoy runtime failure. | [Run 36089689032](https://github.com/centerionware/not-k8s/actions/runs/36089689032) |
| 2026-09-25 | `4a4c75e8fa9dab61e62672a14aa956ef7e16b74b` | Branch-runtime K3s + Cilium | Source setup passed, including Cilium, storage/workloads, and the node-IP wait; forward migration completed. Target Cilium Envoy then failed creating its nested volume mountpoint under a read-only root, preventing hostpath CSI readiness and reverse migration. | [Run 36088034808](https://github.com/centerionware/not-k8s/actions/runs/36088034808) |
| 2026-09-25 | `4a4c75e8fa9dab61e62672a14aa956ef7e16b74b` | Branch-runtime Kubernetes + Cilium | Forward migration completed; target Cilium Envoy mountpoint failed identically, leaving CNI/CoreDNS and hostpath CSI unready. | [Run 36088034808](https://github.com/centerionware/not-k8s/actions/runs/36088034808) |
| 2026-09-25 | `4a4c75e8fa9dab61e62672a14aa956ef7e16b74b` | Five-node Docker preflight | Passed; checks host isolation and capabilities, not Kubernetes or migration parity. | [Run 36088034808](https://github.com/centerionware/not-k8s/actions/runs/36088034808) |
| 2026-09-25 | `315f621fbb90d9d624d26167313740e756691059` | Branch-runtime K3s + Cilium | Failed before migration: K3s API was reachable before the Node registered; fixture's JSONPath query indexed an empty node list. A bounded node-IP wait is added in the worktree. | [Run 36086342991](https://github.com/centerionware/not-k8s/actions/runs/36086342991) |
| 2026-09-25 | `315f621fbb90d9d624d26167313740e756691059` | Branch-runtime Kubernetes + Cilium | Forward migration completed; post-cutover hostpath CSI readiness failed. Cilium Envoy repeatedly exited because runc could not create its volume target under the read-only image root. | [Run 36086342991](https://github.com/centerionware/not-k8s/actions/runs/36086342991) |
| 2026-09-25 | `315f621fbb90d9d624d26167313740e756691059` | Five-node Docker preflight | Passed. This checks isolation and host capabilities only, not Kubernetes or migration parity. | [Run 36086342991](https://github.com/centerionware/not-k8s/actions/runs/36086342991) |
| 2026-09-24 | `daac9ad05285e6ff749d4c51a18f9b71c8fb7dee` | Targeted quick-check: `nodebootstrap,nodemigrate` | Passed; predates bidirectional changes | [Run 35949477611](https://github.com/centerionware/not-k8s/actions/runs/35949477611) |
| 2026-09-24 | `55704b1c202c248ab633aac8c74877c46499e59f` | Nodemigrate crate checks | Failed compile; fixes are in worktree, rerun pending | [Run 35952766075](https://github.com/centerionware/not-k8s/actions/runs/35952766075) |
| 2026-09-24 | `55704b1c202c248ab633aac8c74877c46499e59f` | Targeted quick-check | Failed compile; fixes are in worktree, rerun pending | [Run 35952782893](https://github.com/centerionware/not-k8s/actions/runs/35952782893) |
| 2026-09-24 | `4e932917080e21412c79625fb2b79d08f5e62c0f` | Nodemigrate crate tests | Compiled; 9 passed, upstream control-plane network fixture failed because the manifest path was rooted twice. Fix pending CI rerun. | [Run 35954374767](https://github.com/centerionware/not-k8s/actions/runs/35954374767) |
| 2026-09-24 | `4e932917080e21412c79625fb2b79d08f5e62c0f` | PR shell validation | Passed | [Run 35954374805](https://github.com/centerionware/not-k8s/actions/runs/35954374805) |
| 2026-09-24 | `4e932917080e21412c79625fb2b79d08f5e62c0f` | Commit convention | Passed | [Run 35954372285](https://github.com/centerionware/not-k8s/actions/runs/35954372285) |
| 2026-09-24 | `4ef3585eaea6beae51ffd1116134435b0edc305e` | Nodemigrate crate tests | Passed | [Run 35954606805](https://github.com/centerionware/not-k8s/actions/runs/35954606805) |
| 2026-09-24 | `4ef3585eaea6beae51ffd1116134435b0edc305e` | PR shell validation | Passed | [Run 35954606851](https://github.com/centerionware/not-k8s/actions/runs/35954606851) |
| 2026-09-24 | `4ef3585eaea6beae51ffd1116134435b0edc305e` | Commit convention | Passed | [Run 35954605041](https://github.com/centerionware/not-k8s/actions/runs/35954605041) |
| 2026-09-24 | `63cb20cbe75ebdfafee861c41137b334fb6ff95b` | Nodemigrate crate tests | Passed, including source CNI detection and joined replacement worker command tests | [Run 35955823452](https://github.com/centerionware/not-k8s/actions/runs/35955823452) |
| 2026-09-24 | `63cb20cbe75ebdfafee861c41137b334fb6ff95b` | PR shell validation | Passed | [Run 35955823568](https://github.com/centerionware/not-k8s/actions/runs/35955823568) |
| 2026-09-24 | `63cb20cbe75ebdfafee861c41137b334fb6ff95b` | Commit convention | Passed | [Run 35955821523](https://github.com/centerionware/not-k8s/actions/runs/35955821523) |
| 2026-09-24 | `fc161b8c4262cdeb251abf6bbf2090e180d56c55` | K3s + Cilium round trip | Failed during hostPath CSI setup before nodemigrate was invoked: sandbox CNI selected `bridge`, but `/opt/cni/bin/bridge` was missing. Cilium DaemonSet rollout had completed. | [Run 36034461348](https://github.com/centerionware/not-k8s/actions/runs/36034461348) |
| 2026-09-24 | `fc161b8c4262cdeb251abf6bbf2090e180d56c55` | Upstream Kubernetes + Cilium round trip | Failed before the first checkpoint: Cilium init `config` and `cilium-operator` crashed, and DaemonSet rollout timed out. The uploaded artifact lacks pod container logs; diagnostics now capture them on the next run. | [Run 36034461348](https://github.com/centerionware/not-k8s/actions/runs/36034461348) |
| 2026-09-24 | `c651731652298f73540ba71a83b3c4cf6972bf64` | K3s + Cilium round trip | Failed before migration: runtime selected stale Podman bridge CNI config with absent `bridge`/`loopback` binaries, and CSI could not mount `/var/lib/nodelet/plugins`. Follow-up aligns Cilium CNI paths and creates plugin directories. | [Run 36037082232](https://github.com/centerionware/not-k8s/actions/runs/36037082232) |
| 2026-09-24 | `c651731652298f73540ba71a83b3c4cf6972bf64` | Upstream Kubernetes + Cilium round trip | Failed before migration: Cilium's API client used `127.0.0.1`, which is not in kubeadm's API certificate SANs. Follow-up uses the node's advertised InternalIP. | [Run 36037082232](https://github.com/centerionware/not-k8s/actions/runs/36037082232) |
| 2026-09-24 | `0655d0aacb6e7d90ee221061e7172842d63fcc99` | K3s + Cilium round trip | Failed in fixture dependency setup before installing K3s: the script searched `/usr/lib/cni`, but the runner's `containernetworking-plugins` package installed the binaries elsewhere. | [Run 36039647520](https://github.com/centerionware/not-k8s/actions/runs/36039647520) |
| 2026-09-24 | `0655d0aacb6e7d90ee221061e7172842d63fcc99` | Upstream Kubernetes + Cilium round trip | Failed in fixture dependency setup before kubeadm init for the same CNI plugin directory assumption. | [Run 36039647520](https://github.com/centerionware/not-k8s/actions/runs/36039647520) |
| 2026-09-24 | `458c52c6247dfab8479cb18f33ae04a3db5b8c8d` | K3s + Cilium round trip | Cilium and the CSI pods ran, but the CSI driver was not registered in the active kubelet's plugin registry; its missing topology keys kept the test PVC Pending. No migration command ran. | [Run 36040401537](https://github.com/centerionware/not-k8s/actions/runs/36040401537) |
| 2026-09-24 | `458c52c6247dfab8479cb18f33ae04a3db5b8c8d` | Upstream Kubernetes + Cilium round trip | Failed before kubeadm init: the preinstalled `containernetworking-plugins` package conflicted with kubelet's `kubernetes-cni` dependency. No migration command ran. | [Run 36040401537](https://github.com/centerionware/not-k8s/actions/runs/36040401537) |
| 2026-09-24 | `458c52c6247dfab8479cb18f33ae04a3db5b8c8d` | Five-node Docker preflight | Docker image build passed; probe failed because `rw=true` is not a valid `--mount` field. | [Run 36040401537](https://github.com/centerionware/not-k8s/actions/runs/36040401537) |
| 2026-09-24 | Worktree after `458c52c6` | Migration fixture follow-up | Separates CNI packages by source distribution, selects `/var/lib/kubelet` or `/var/lib/nodelet` for CSI driver setup at each stage, and removes the invalid Docker mount option. Shell/runtime validation is pending. | — |
| 2026-09-24 | `ef04b0e8c7e59b6f65bb41564139325d7026fdb9` | K3s + Cilium round trip | Cilium, CSI deployment, and CSI readiness PVC passed. The archived full setup then failed waiting for DRA registration, which requires nodelet and is not part of a native K3s source. Migration was not invoked. The package's `/opt/cni/bin` files were also replaced with self-links, causing transient bridge errors. | [Run 36042056929](https://github.com/centerionware/not-k8s/actions/runs/36042056929) |
| 2026-09-24 | `ef04b0e8c7e59b6f65bb41564139325d7026fdb9` | Upstream Kubernetes + Cilium round trip | Kubeadm/Cilium, CSI deployment, and CSI readiness PVC passed. The archived full setup then failed waiting for nodelet DRA registration on native kubelet. Migration was not invoked. | [Run 36042056929](https://github.com/centerionware/not-k8s/actions/runs/36042056929) |
| 2026-09-24 | `ef04b0e8c7e59b6f65bb41564139325d7026fdb9` | Five-node Docker preflight | Docker image build passed; systemd did not become ready in the first node container. The probe now records container state and logs on this failure. | [Run 36042056929](https://github.com/centerionware/not-k8s/actions/runs/36042056929) |
| 2026-09-24 | Worktree after `ef04b0e8` | Migration fixture follow-up | Stops the archived setup after its CSI section, avoids replacing `/opt/cni/bin` entries with self-links, and captures Docker startup diagnostics. Shell/runtime validation is pending. | — |
| 2026-09-24 | `580ae951144e0f2aa753a06e93e7a3f10264da75` | K3s + Cilium round trip | Source cluster Cilium, CSI and PVCs, cert-manager, Traefik, nginx, data, and certificate checks passed. The first nodemigrate call aborted because rustls had both `aws-lc-rs` and `ring` enabled with no process default installed. No transfer completed. | [Run 36043310369](https://github.com/centerionware/not-k8s/actions/runs/36043310369) |
| 2026-09-24 | `580ae951144e0f2aa753a06e93e7a3f10264da75` | Upstream Kubernetes + Cilium round trip | Kubeadm, Cilium and CSI readiness passed; cert-manager deployed and Traefik's pod was Running, but Helm `--wait` timed out after 10 minutes. Migration was not invoked. | [Run 36043310369](https://github.com/centerionware/not-k8s/actions/runs/36043310369) |
| 2026-09-24 | `580ae951144e0f2aa753a06e93e7a3f10264da75` | Five-node Docker preflight | The image built; the first container exited with code 255 before systemd readiness. `docker logs` was empty. | [Run 36043310369](https://github.com/centerionware/not-k8s/actions/runs/36043310369) |
| 2026-09-24 | Worktree after `580ae951` | Runtime follow-up | Selects the rustls ring provider before the utility runs, waits for Traefik with a Kubernetes Deployment rollout check, and starts systemd with console logs while retaining private cgroup namespaces. Shell/runtime validation is pending. | — |
| 2026-09-24 | `cefadddd0fa51dfc1a10d535160f1fc64a316b75` | K3s + Cilium round trip | All source-stage checks passed and nodemigrate ran. The explicit rustls provider fixed the earlier panic, but Tower aborted because no Tokio reactor was entered while constructing the Kubernetes client. No transfer completed. | [Run 36045632585](https://github.com/centerionware/not-k8s/actions/runs/36045632585) |
| 2026-09-24 | `cefadddd0fa51dfc1a10d535160f1fc64a316b75` | Upstream Kubernetes + Cilium round trip | All source-stage checks passed, including Traefik rollout and route. Nodemigrate ran and hit the same Tower/Tokio runtime panic before transfer. | [Run 36045632585](https://github.com/centerionware/not-k8s/actions/runs/36045632585) |
| 2026-09-24 | `cefadddd0fa51dfc1a10d535160f1fc64a316b75` | Five-node Docker preflight | Image build passed; the first container still exited 255 and emitted no systemd console output despite log-target configuration. | [Run 36045632585](https://github.com/centerionware/not-k8s/actions/runs/36045632585) |
| 2026-09-24 | Worktree after `cefadddd` | Runtime follow-up | Enters the Tokio runtime while building the Kubernetes API client and adds a focused regression test. The Docker entrypoint now prints the resolved systemd path and enables debug console logs. Shell/runtime validation pending. | — |
| 2026-09-24 | `1c42edc60fe45a5af654dd3a3f6d055047eacaff` | Focused nodemigrate tests | 49 passed; the new runtime-context regression did not reach client creation because Rust's escaped string removed YAML indentation. Fixture corrected to raw YAML; retest pending. | [Run 36046895251](https://github.com/centerionware/not-k8s/actions/runs/36046895251) |
| 2026-09-24 | `1c42edc60fe45a5af654dd3a3f6d055047eacaff` | K3s + Cilium migration | Source-stage checks passed and the warning printed. Target bootstrap unexpectedly enabled flanneld, which never wrote its subnet. Need resolve why source detection selected flannel instead of external Cilium. | [Run 36046899524](https://github.com/centerionware/not-k8s/actions/runs/36046899524) |
| 2026-09-24 | `65e212ec976fd68cbb5e97645c6e6bccd2158fe6` | Focused nodemigrate tests | Passed, including the runtime-context regression with corrected raw YAML. | [Run 36048275119](https://github.com/centerionware/not-k8s/actions/runs/36048275119) |
| 2026-09-24 | `bd0885951771fcc917d7b5a4a8ae01f3a0e148b8` | Nodemigrate tests, checks, and release | All three passed; the Tokio-context test initializes the same ring provider as the binary entrypoint. | [Tests 36048798023](https://github.com/centerionware/not-k8s/actions/runs/36048798023), [checks 36048797939](https://github.com/centerionware/not-k8s/actions/runs/36048797939), [release 36048797913](https://github.com/centerionware/not-k8s/actions/runs/36048797913) |
| 2026-09-24 | `bd0885951771fcc917d7b5a4a8ae01f3a0e148b8` | K3s + Cilium migration | Source checks and warning passed; nodemigrate again enabled flanneld and failed waiting for subnet. The selected-CNI ordering is being corrected to honor active Cilium. | [Run 36048845512](https://github.com/centerionware/not-k8s/actions/runs/36048845512) |
| 2026-09-24 | `bd0885951771fcc917d7b5a4a8ae01f3a0e148b8` | Upstream Kubernetes + Cilium migration | Source checks and warning passed; nodeapiserver readiness failed because port 6443 remained bound, with nodestore client-certificate errors. | [Run 36048845512](https://github.com/centerionware/not-k8s/actions/runs/36048845512) |
| 2026-09-24 | `bd0885951771fcc917d7b5a4a8ae01f3a0e148b8` | Five-node Docker preflight | Image build passed; systemd exited 255 before readiness with no further output. | [Run 36048845512](https://github.com/centerionware/not-k8s/actions/runs/36048845512) |
| 2026-09-24 | `1c42edc60fe45a5af654dd3a3f6d055047eacaff` | Upstream Kubernetes + Cilium migration | Source-stage checks passed and the warning printed. Target nodeapiserver could not bind 6443; nodestore logged port-in-use and UnknownIssuer errors. No transfer completed. | [Run 36046899524](https://github.com/centerionware/not-k8s/actions/runs/36046899524) |
| 2026-09-24 | `1c42edc60fe45a5af654dd3a3f6d055047eacaff` | Five-node Docker preflight | Image build passed; systemd executable was resolved to `/usr/lib/systemd/systemd`, but the container exited 255 with no further output. | [Run 36046899524](https://github.com/centerionware/not-k8s/actions/runs/36046899524) |
| 2026-09-24 | Worktree after `bd088595` | Follow-up | Selects an active external CNI before defaulting to K3s Flannel, with a regression case for Cilium and its Flannel backup file. Upstream static-pod cleanup/port handoff and Docker systemd startup remain under investigation. | — |
| — | — | Existing-cluster join/replacement | Not run; scenario not yet exercised by current script | — |
| — | — | Per-stage Cilium health assertions | Added to the script; static syntax passed, runtime lanes pending | [Run 35956267629](https://github.com/centerionware/not-k8s/actions/runs/35956267629) |
| 2026-09-24 | `21d532991835ff587a918e3bf665ed4f601e6fdf` | Nodemigrate crate tests | Passed | [Run 35956267726](https://github.com/centerionware/not-k8s/actions/runs/35956267726) |
| 2026-09-24 | `21d532991835ff587a918e3bf665ed4f601e6fdf` | PR shell validation | Passed, including `bash -n` for the Cilium checkpoint assertions | [Run 35956267629](https://github.com/centerionware/not-k8s/actions/runs/35956267629) |
| 2026-09-24 | `21d532991835ff587a918e3bf665ed4f601e6fdf` | Commit convention | Passed | [Run 35956264583](https://github.com/centerionware/not-k8s/actions/runs/35956264583) |
| 2026-09-24 | `8518a99537cc4b98f2cbf8184f49c9927a8d78cf` | Nodemigrate crate checks | Passed | [Run 35956413580](https://github.com/centerionware/not-k8s/actions/runs/35956413580) |
| 2026-09-24 | `8518a99537cc4b98f2cbf8184f49c9927a8d78cf` | PR shell validation | Passed | [Run 35956413566](https://github.com/centerionware/not-k8s/actions/runs/35956413566) |
| 2026-09-24 | `8518a99537cc4b98f2cbf8184f49c9927a8d78cf` | Commit convention | Passed | [Run 35956412012](https://github.com/centerionware/not-k8s/actions/runs/35956412012) |
| 2026-09-24 | `c468627045cae98360bed8a93397407ff934ed86` | Nodemigrate crate checks | Passed | [Run 35957207376](https://github.com/centerionware/not-k8s/actions/runs/35957207376) |
| 2026-09-24 | `c468627045cae98360bed8a93397407ff934ed86` | Quick-check: `nodebootstrap,nodestore` | Passed | [Run 35957212012](https://github.com/centerionware/not-k8s/actions/runs/35957212012) |
| 2026-09-24 | `c468627045cae98360bed8a93397407ff934ed86` | PR shell validation | Passed | [Run 35957207356](https://github.com/centerionware/not-k8s/actions/runs/35957207356) |
| 2026-09-24 | `c468627045cae98360bed8a93397407ff934ed86` | Commit convention | Passed | [Run 35957206090](https://github.com/centerionware/not-k8s/actions/runs/35957206090) |
| 2026-09-24 | `2db20f9cc84ec3146197571807387fb1141972e3` | Nodemigrate crate checks | Passed | [Run 35957514584](https://github.com/centerionware/not-k8s/actions/runs/35957514584) |
| 2026-09-24 | `2db20f9cc84ec3146197571807387fb1141972e3` | PR shell validation | Passed | [Run 35957514687](https://github.com/centerionware/not-k8s/actions/runs/35957514687) |
| 2026-09-24 | `2db20f9cc84ec3146197571807387fb1141972e3` | Commit convention | Passed | [Run 35957511809](https://github.com/centerionware/not-k8s/actions/runs/35957511809) |
| 2026-09-24 | `421fae64140eea5eb6588a923d9c31663dda9caf` | Nodemigrate crate tests and packaging checks | Passed after adding K3s-agent and upstream-worker role inventory; does not verify worker cutover | [Crate run 35959159615](https://github.com/centerionware/not-k8s/actions/runs/35959159615) |
| 2026-09-24 | `421fae64140eea5eb6588a923d9c31663dda9caf` | PR shell validation | Passed | [Run 35959159529](https://github.com/centerionware/not-k8s/actions/runs/35959159529) |
| 2026-09-24 | `421fae64140eea5eb6588a923d9c31663dda9caf` | Commit convention | Passed | [Run 35959155805](https://github.com/centerionware/not-k8s/actions/runs/35959155805) |
| 2026-09-24 | `75f585a69e846e096832162b765626f97e46635a` | Nodemigrate crate tests and packaging checks | Passed; covers joined worker cutover validation and same-name replacement guard | [Run 35959544178](https://github.com/centerionware/not-k8s/actions/runs/35959544178) |
| 2026-09-24 | `75f585a69e846e096832162b765626f97e46635a` | PR shell validation | Passed | [Run 35959544161](https://github.com/centerionware/not-k8s/actions/runs/35959544161) |
| 2026-09-24 | `75f585a69e846e096832162b765626f97e46635a` | Commit convention | Passed | [Run 35959542626](https://github.com/centerionware/not-k8s/actions/runs/35959542626) |
| 2026-09-24 | `5dbd1c36f9fed695755b26d01b03fcf5f94306b7` | Nodemigrate crate checks | Failed to compile because `Distribution` was imported through a private module path; corrected in the follow-up commit | [Run 35959962403](https://github.com/centerionware/not-k8s/actions/runs/35959962403) |
| 2026-09-24 | `5dbd1c36f9fed695755b26d01b03fcf5f94306b7` | PR shell validation | Passed | [Run 35959962370](https://github.com/centerionware/not-k8s/actions/runs/35959962370) |
| 2026-09-24 | `5dbd1c36f9fed695755b26d01b03fcf5f94306b7` | Commit convention | Passed | [Run 35959960601](https://github.com/centerionware/not-k8s/actions/runs/35959960601) |
| 2026-09-24 | `e1a304e79b4e205254e55ff5b531c74b9fea05d5` | Nodemigrate crate tests and packaging checks | Passed; includes the static-pod selection and CRI endpoint fixtures | [Run 35960125413](https://github.com/centerionware/not-k8s/actions/runs/35960125413) |
| 2026-09-24 | `e1a304e79b4e205254e55ff5b531c74b9fea05d5` | PR shell validation | Passed | [Run 35960125424](https://github.com/centerionware/not-k8s/actions/runs/35960125424) |
| 2026-09-24 | `e1a304e79b4e205254e55ff5b531c74b9fea05d5` | Commit convention | Passed | [Run 35960121002](https://github.com/centerionware/not-k8s/actions/runs/35960121002) |
| 2026-09-24 | `d2ff8a5da8f9ebd988733e61a309caff7d9f3a17` | Nodemigrate crate tests and packaging checks | Passed; includes per-worker hostPath/local PV collection and filtering | [Run 35960475945](https://github.com/centerionware/not-k8s/actions/runs/35960475945) |
| 2026-09-24 | `d2ff8a5da8f9ebd988733e61a309caff7d9f3a17` | PR shell validation | Passed | [Run 35960475953](https://github.com/centerionware/not-k8s/actions/runs/35960475953) |
| 2026-09-24 | `d2ff8a5da8f9ebd988733e61a309caff7d9f3a17` | Commit convention | Passed | [Run 35960474053](https://github.com/centerionware/not-k8s/actions/runs/35960474053) |
| 2026-09-24 | `c2129d8868d96be7f7d373249af80faed146e1d1` | Nodemigrate crate tests and packaging checks | Passed; covers not-k8s nodelet worker inventory and reverse worker path validation | [Run 35960872299](https://github.com/centerionware/not-k8s/actions/runs/35960872299) |
| 2026-09-24 | `c2129d8868d96be7f7d373249af80faed146e1d1` | PR shell validation | Passed | [Run 35960872315](https://github.com/centerionware/not-k8s/actions/runs/35960872315) |
| 2026-09-24 | `c2129d8868d96be7f7d373249af80faed146e1d1` | Commit convention | Passed | [Run 35960871013](https://github.com/centerionware/not-k8s/actions/runs/35960871013) |
| 2026-09-24 | `84b4493818b61fdeb3efcf2a6935a30ebd7eaf31` | Nodemigrate crate tests and packaging checks | Passed; includes all inventory, worker forward/return, and static-pod fixture coverage present at this SHA | [Run 35961085117](https://github.com/centerionware/not-k8s/actions/runs/35961085117) |
| 2026-09-24 | `84b4493818b61fdeb3efcf2a6935a30ebd7eaf31` | PR shell validation | Passed | [Run 35961085137](https://github.com/centerionware/not-k8s/actions/runs/35961085137) |
| 2026-09-24 | `84b4493818b61fdeb3efcf2a6935a30ebd7eaf31` | Commit convention | Passed | [Run 35961083449](https://github.com/centerionware/not-k8s/actions/runs/35961083449) |
| 2026-09-24 | `a608ce684a2cc5147852f1de8929fb6efadaa440` | PR shell validation | Passed; manual migration job was skipped because this was a pull request | [Run 35961786448](https://github.com/centerionware/not-k8s/actions/runs/35961786448) |
| 2026-09-24 | `a608ce684a2cc5147852f1de8929fb6efadaa440` | Nodemigrate packaging checks | Passed | [Run 35961786460](https://github.com/centerionware/not-k8s/actions/runs/35961786460) |
| 2026-09-24 | `a608ce684a2cc5147852f1de8929fb6efadaa440` | Commit convention | Passed | [Run 35961784865](https://github.com/centerionware/not-k8s/actions/runs/35961784865) |
| 2026-09-24 | `41472ae16cef3b6986b3f43d5bf0410ddd3c9062` | PR shell validation | Passed; migration job remained skipped on pull request | [Run 35961904181](https://github.com/centerionware/not-k8s/actions/runs/35961904181) |
| 2026-09-24 | `41472ae16cef3b6986b3f43d5bf0410ddd3c9062` | Nodemigrate packaging checks | Passed | [Run 35961904142](https://github.com/centerionware/not-k8s/actions/runs/35961904142) |
| 2026-09-24 | `41472ae16cef3b6986b3f43d5bf0410ddd3c9062` | Commit convention | Passed | [Run 35961901937](https://github.com/centerionware/not-k8s/actions/runs/35961901937) |
| 2026-09-24 | `0fe454dad5b3fb194b72a48bc657ed0512a7f29a` | Nodemigrate crate checks | Passed, including skip-import request and control-plane joined-cluster validation | [Run 35962922792](https://github.com/centerionware/not-k8s/actions/runs/35962922792) |
| 2026-09-24 | `0fe454dad5b3fb194b72a48bc657ed0512a7f29a` | PR shell validation | Passed; the manual migration runtime job was skipped on pull request | [Run 35962922860](https://github.com/centerionware/not-k8s/actions/runs/35962922860) |
| 2026-09-24 | `0fe454dad5b3fb194b72a48bc657ed0512a7f29a` | Commit convention | Passed | [Run 35962919894](https://github.com/centerionware/not-k8s/actions/runs/35962919894) |
| 2026-09-24 | `0c4236f9467028f93f12f2736a41db8ab0950b71` | Nodemigrate crate checks | Failed: 29 passed and `rejects_staging_with_uninstall_and_conflicting_reverse_options` failed because parser accepted simultaneous `stage-target` and `skip-api-export`; fixed in `7e167545`. | [Run 35965244870](https://github.com/centerionware/not-k8s/actions/runs/35965244870) |
| 2026-09-24 | `0c4236f9467028f93f12f2736a41db8ab0950b71` | PR shell validation and commit convention | Passed | [Shell run 35965245062](https://github.com/centerionware/not-k8s/actions/runs/35965245062), [commit run 35965242770](https://github.com/centerionware/not-k8s/actions/runs/35965242770) |
| 2026-09-24 | `7e16754501e87d5983dcb0335b6d972529dec5b9` | Nodemigrate crate checks | Passed; now rejects the conflicting reverse-mode request | [Run 35965492161](https://github.com/centerionware/not-k8s/actions/runs/35965492161) |
| 2026-09-24 | `7e16754501e87d5983dcb0335b6d972529dec5b9` | PR shell validation | Passed; manual migration runtime job skipped on pull request | [Run 35965492244](https://github.com/centerionware/not-k8s/actions/runs/35965492244) |
| 2026-09-24 | `7e16754501e87d5983dcb0335b6d972529dec5b9` | Commit convention | Passed | [Run 35965490640](https://github.com/centerionware/not-k8s/actions/runs/35965490640) |
| 2026-09-24 | `a4f4264ae3dbf3334c6c58d620d9dc4f35f1e966` | Nodemigrate crate checks | Passed, including source/destination stale-Node replacement validation | [Run 35966523279](https://github.com/centerionware/not-k8s/actions/runs/35966523279) |
| 2026-09-24 | `a4f4264ae3dbf3334c6c58d620d9dc4f35f1e966` | Integration shell validation | Passed; manual migration runtime job skipped on pull request | [Run 35966522595](https://github.com/centerionware/not-k8s/actions/runs/35966522595) |
| 2026-09-24 | `a4f4264ae3dbf3334c6c58d620d9dc4f35f1e966` | Commit convention | Passed | [Run 35966519686](https://github.com/centerionware/not-k8s/actions/runs/35966519686) |
| 2026-09-24 | `b522fbf1dbd66c08ad76675f6f693dad2fce0b8c` | Nodemigrate crate checks | Failed to compile: two host-path snapshot calls passed `Option<&&HashMap>` where `Option<&HashMap>` is required; corrected by removing `.as_ref()` | [Run 35967706704](https://github.com/centerionware/not-k8s/actions/runs/35967706704) |
| 2026-09-24 | `1615131d4141fb6701588e58833b203d8850aa42` | Nodemigrate crate checks | Passed; covers scheduling-state extraction and migration validation | [Run 35967908377](https://github.com/centerionware/not-k8s/actions/runs/35967908377) |
| 2026-09-24 | `1615131d4141fb6701588e58833b203d8850aa42` | Integration shell validation | Passed; manual migration runtime job skipped on pull request | [Run 35967908448](https://github.com/centerionware/not-k8s/actions/runs/35967908448) |
| 2026-09-24 | `1615131d4141fb6701588e58833b203d8850aa42` | Commit convention | Passed | [Run 35967906382](https://github.com/centerionware/not-k8s/actions/runs/35967906382) |
| 2026-09-24 | `2a9b26c3061e86c29d9f601b3bc7a461a8e718dc` | Nodemigrate crate checks | Passed, including state extraction and reverse confirmation validation | [Run 35968225182](https://github.com/centerionware/not-k8s/actions/runs/35968225182) |
| 2026-09-24 | `2a9b26c3061e86c29d9f601b3bc7a461a8e718dc` | Integration shell validation | Passed; manual migration runtime job skipped on pull request | [Run 35968225196](https://github.com/centerionware/not-k8s/actions/runs/35968225196) |
| 2026-09-24 | `2a9b26c3061e86c29d9f601b3bc7a461a8e718dc` | Commit convention | Passed | [Run 35968221928](https://github.com/centerionware/not-k8s/actions/runs/35968221928) |
| 2026-09-24 | `caed49582952a0743d87a06bbb52fceb737c059e` | Targeted nodemigrate checks | Failed in the crate-tests job; packaging and crate detection passed. GitHub log retrieval failed with an API connection error, so the diagnostic is not yet available. | [Run 35972976396](https://github.com/centerionware/not-k8s/actions/runs/35972976396) |
| 2026-09-24 | `a8ec8a744de3c442d1dd0d303f696cae7d05457c` | Integration shell validation | Passed; covers shell syntax and snapshot normalization. Manual runtime jobs were skipped for the pull request. | [Run 35973565436](https://github.com/centerionware/not-k8s/actions/runs/35973565436) |
| 2026-09-24 | `f791d0e37440a5392bab05c22ece423f69fed7ad` | Targeted nodemigrate checks | Failed again in crate tests; packaging and detection passed. The workflow uploaded artifact `nodemigrate-tests-35975029524` with the full log, but this host could not download it. | [Run 35975029524](https://github.com/centerionware/not-k8s/actions/runs/35975029524) |
| 2026-09-24 | `f791d0e37440a5392bab05c22ece423f69fed7ad` | Release policy | Passed; standalone nodemigrate publication uses `--latest=false` and the policy checker enforces it. | [Run 35975029509](https://github.com/centerionware/not-k8s/actions/runs/35975029509) |
| 2026-09-24 | `f791d0e37440a5392bab05c22ece423f69fed7ad` | Integration shell validation | Passed; covers shell syntax and snapshot normalization. Manual runtime jobs were skipped for the pull request. | [Run 35975029519](https://github.com/centerionware/not-k8s/actions/runs/35975029519) |
| 2026-09-24 | `e73dbaaef0f07a0bf80c3fc57555d13f649828a0` | Targeted nodemigrate checks | Failed one of 38 tests: the manifest round-trip fixture's temporary directory was mode `0755`, and the loader correctly refused it. This also showed the path-traversal fixture was passing early on the directory-permission check. Both fixtures now set mode `0700`. | [Run 35976175321](https://github.com/centerionware/not-k8s/actions/runs/35976175321) |
| 2026-09-24 | `e943bb756ef8568c4e4bd89ee2bb6d16c165107f` | Targeted nodemigrate checks | Passed: 38 crate tests, packaging policy, and crate detection. | [Run 35976429487](https://github.com/centerionware/not-k8s/actions/runs/35976429487) |
| 2026-09-24 | `e943bb756ef8568c4e4bd89ee2bb6d16c165107f` | Release policy | Passed. | [Run 35976429468](https://github.com/centerionware/not-k8s/actions/runs/35976429468) |
| 2026-09-24 | `e943bb756ef8568c4e4bd89ee2bb6d16c165107f` | Integration shell validation | Passed; the manual runtime matrix was skipped for the pull request. | [Run 35976429428](https://github.com/centerionware/not-k8s/actions/runs/35976429428) |
| 2026-09-24 | `bb82cea88f48d73ca0bbb5ee00c0fd35a00fac20` | Targeted nodemigrate checks | Failed to compile: the new CNI backup field was missing from `Export`, and a path iterator attempted to clone `Path`. Both were corrected in follow-up commits. | [Run 36025781690](https://github.com/centerionware/not-k8s/actions/runs/36025781690) |
| 2026-09-24 | `bb82cea88f48d73ca0bbb5ee00c0fd35a00fac20` | Quick-check `nodebootstrap` | Passed; covers the CNI containerd config table update. The nodebootstrap crate was unchanged in later nodemigrate-only fixes. | [Run 36025823559](https://github.com/centerionware/not-k8s/actions/runs/36025823559) |
| 2026-09-24 | `60711b0052b4c8417d44926757a4bd7a7a1d4386` | Targeted nodemigrate checks | Compiled, but two K3s CNI fixtures failed because detection no longer used `/etc/cni/net.d`; the established fallback was restored. | [Run 36026059417](https://github.com/centerionware/not-k8s/actions/runs/36026059417) |
| 2026-09-24 | `c2df44bfd41f7e34f5eb92ce52a2196eb36849a3` | Targeted nodemigrate checks | Passed crate tests, crate detection, and packaging checks, including K3s Cilium path detection and CNI directory recovery after data-dir removal. | [Run 36026420825](https://github.com/centerionware/not-k8s/actions/runs/36026420825) |
| 2026-09-24 | `c2df44bfd41f7e34f5eb92ce52a2196eb36849a3` | PR validation and release policy | Passed shell/PR validation, commit convention, and the no-latest release policy check. General build and migration runtime jobs were skipped as requested. | [Validation run 36026420780](https://github.com/centerionware/not-k8s/actions/runs/36026420780), [commit run 36026417043](https://github.com/centerionware/not-k8s/actions/runs/36026417043), [policy run 36026420914](https://github.com/centerionware/not-k8s/actions/runs/36026420914) |
| 2026-09-24 | `ac5a05b4` | Targeted nodemigrate checks | Passed crate tests, packaging, and crate detection; verifies v2 export node-state serialization and load. The subsequent worker offline-join validation is pending on `08fb393c`. | [Run 36028447623](https://github.com/centerionware/not-k8s/actions/runs/36028447623) |
| 2026-09-24 | `08fb393c` | Targeted nodemigrate checks | Passed crate tests, packaging, and crate detection; includes offline worker join validation. Runtime quorum-loss and per-node volume recovery remain unverified. | [Run 36028759134](https://github.com/centerionware/not-k8s/actions/runs/36028759134) |
| 2026-09-24 | `a360ea6c` | Targeted nodemigrate checks | Passed crate tests, packaging, and crate detection; includes node-affined per-node hostPath backup and restore coverage. Real quorum-loss migration remains unverified. | [Run 36029431383](https://github.com/centerionware/not-k8s/actions/runs/36029431383) |
| 2026-09-24 | `ad509e5b` | Targeted nodemigrate checks and PR validation | Crate tests, packaging, detection, release policy, commit convention, and shell validation passed. Docker isolation preflight and migration runtime jobs were skipped on the pull request. | [Nodemigrate run 36031941620](https://github.com/centerionware/not-k8s/actions/runs/36031941620), [validation run 36031941811](https://github.com/centerionware/not-k8s/actions/runs/36031941811), [policy run 36031941938](https://github.com/centerionware/not-k8s/actions/runs/36031941938), [commit run 36031938595](https://github.com/centerionware/not-k8s/actions/runs/36031938595) |
| 2026-09-24 | `fc161b8c4262cdeb251abf6bbf2090e180d56c55` | Release-backed migration workflow | Building `nodemigrate`, fetching the latest regular combined runtime `v0.8.0`, verifying its digest, and checking components passed. K3s and kubeadm lanes failed during CNI/Cilium setup before nodemigrate was invoked. Docker preflight failed because the Dockerfile path was relative to the wrong directory; fixed in the follow-up. | [Run 36034461348](https://github.com/centerionware/not-k8s/actions/runs/36034461348) |
| 2026-09-24 | `c651731652298f73540ba71a83b3c4cf6972bf64` | Targeted nodemigrate checks | Crate tests, packaging, and crate detection passed, including interactive risk confirmation and noninteractive warning behavior. | [Run 36037058359](https://github.com/centerionware/not-k8s/actions/runs/36037058359) |
| 2026-09-24 | `c651731652298f73540ba71a83b3c4cf6972bf64` | Release-backed migration workflow | PR utility build and v0.8.0 runtime digest/component checks passed. Both migration lanes failed during CNI/Cilium/CSI setup before nodemigrate ran; the Docker preflight failed because Ubuntu's `bpftool` name has no package candidate. Follow-up fixes remain unverified. | [Run 36037082232](https://github.com/centerionware/not-k8s/actions/runs/36037082232) |
| 2026-09-24 | `0655d0aacb6e7d90ee221061e7172842d63fcc99` | Targeted nodemigrate checks | Crate tests, packaging, and crate detection passed, including interactive risk confirmation and noninteractive warning behavior. | [Run 36039622199](https://github.com/centerionware/not-k8s/actions/runs/36039622199) |
| 2026-09-24 | `0655d0aacb6e7d90ee221061e7172842d63fcc99` | Release-backed migration workflow | PR utility build and v0.8.0 runtime digest/component checks passed. Both lanes stopped in CNI plugin prerequisite setup; the Docker node image built, but its probe failed on invalid bind-mount syntax. | [Run 36039647520](https://github.com/centerionware/not-k8s/actions/runs/36039647520) |
| 2026-09-24 | `458c52c6247dfab8479cb18f33ae04a3db5b8c8d` | Release-backed migration workflow | PR utility build and v0.8.0 runtime digest/component checks passed. K3s reached CSI but no topology keys registered; kubeadm hit the CNI package conflict; Docker probe rejected `rw=true`. No migration lane invoked nodemigrate. Current fixes are pending another manual run. | [Run 36040401537](https://github.com/centerionware/not-k8s/actions/runs/36040401537) |
| 2026-09-24 | `ef04b0e8c7e59b6f65bb41564139325d7026fdb9` | Release-backed migration workflow | PR utility build and v0.8.0 digest/component verification passed. Both lanes passed Cilium and CSI readiness, then the all-in-one setup failed on its unrelated nodelet DRA registration check; K3s CNI plugin symlinks also broke during setup. Docker image built but systemd did not become ready. No lane invoked nodemigrate. | [Run 36042056929](https://github.com/centerionware/not-k8s/actions/runs/36042056929) |
| 2026-09-24 | `580ae951144e0f2aa753a06e93e7a3f10264da75` | Release-backed migration workflow | Utility build and v0.8.0 verification passed. K3s reached nodemigrate, which aborted on rustls provider ambiguity before transfer. Upstream stopped on Helm's Traefik wait; Docker node exited 255 before systemd readiness. Current runtime fixes are pending. | [Run 36043310369](https://github.com/centerionware/not-k8s/actions/runs/36043310369) |
| 2026-09-24 | `cefadddd0fa51dfc1a10d535160f1fc64a316b75` | Release-backed migration workflow | Utility build and v0.8.0 verification passed. Both source stages passed and both migration commands ran; selected rustls provider fixed the first panic, then Tower aborted because the Tokio runtime was not entered during client construction. Docker image built, but systemd still exited 255 without output. | [Run 36045632585](https://github.com/centerionware/not-k8s/actions/runs/36045632585) |
| 2026-09-24 | `29797f3f04489425e06b0f1b57bb0e4e619c0e3e` | Focused nodemigrate checks | Unit tests, targeted checks, release verification, and commit convention passed. | [Tests 36050046938](https://github.com/centerionware/not-k8s/actions/runs/36050046938), [checks 36050046887](https://github.com/centerionware/not-k8s/actions/runs/36050046887), [release 36050046915](https://github.com/centerionware/not-k8s/actions/runs/36050046915) |
| 2026-09-24 | `29797f3f04489425e06b0f1b57bb0e4e619c0e3e` | K3s + Cilium migration | Cilium target bootstrap passed without starting flanneld; the warning printed. API import then rejected 506 objects missing `apiVersion`. | [Run 36050278348](https://github.com/centerionware/not-k8s/actions/runs/36050278348) |
| 2026-09-24 | `29797f3f04489425e06b0f1b57bb0e4e619c0e3e` | Upstream Kubernetes + Cilium migration | Nodeapiserver target bootstrap passed and the warning printed. API import then rejected 488 objects missing `apiVersion`. | [Run 36050278348](https://github.com/centerionware/not-k8s/actions/runs/36050278348) |
| 2026-09-24 | `29797f3f04489425e06b0f1b57bb0e4e619c0e3e` | Five-node Docker preflight | Image build passed; systemd exited 255 before readiness with no further output. | [Run 36050278348](https://github.com/centerionware/not-k8s/actions/runs/36050278348) |
| 2026-09-24 | Worktree after `29797f3f` | Follow-up | Restores API version and kind from discovery when exporting dynamic Kubernetes objects, with a focused regression test. Retest pending. | — |
| 2026-09-24 | `c96505bdaae6387bb825c98faf77d2f2506c087c` | K3s + Cilium migration | Source Cilium, CSI/PVC data, cert-manager, Traefik, and nginx checks passed; warning printed and target bootstrapped without flanneld. Import stopped with 37 objects pending because destination discovery lacked `snapshot.storage.k8s.io/v1/VolumeSnapshotClass`. | [Run 36051665474](https://github.com/centerionware/not-k8s/actions/runs/36051665474) |
| 2026-09-24 | `c96505bdaae6387bb825c98faf77d2f2506c087c` | Upstream Kubernetes + Cilium migration | Source checks and warning passed; nodeapiserver bootstrapped. Import stopped with 26 objects pending because destination discovery lacked `cilium.io/v2/CiliumEndpoint`. TLS errors appeared in post-failure diagnostics, not as the reported import error. | [Run 36051665474](https://github.com/centerionware/not-k8s/actions/runs/36051665474) |
| 2026-09-24 | `c96505bdaae6387bb825c98faf77d2f2506c087c` | Five-node Docker preflight | Image build passed; first systemd container exited 255 before readiness with no further output. | [Run 36051665474](https://github.com/centerionware/not-k8s/actions/runs/36051665474) |
| 2026-09-24 | `3fb149bc59b56cddc3b5f962215c738ad26e3b7d` | Focused nodemigrate checks | Crate tests, including grouped API-restore failure reporting, passed. Packaging passed. | [Run 36053395357](https://github.com/centerionware/not-k8s/actions/runs/36053395357) |
| 2026-09-24 | `3fb149bc59b56cddc3b5f962215c738ad26e3b7d` | PR shell validation and commit convention | Both passed. | [Shell run 36053395366](https://github.com/centerionware/not-k8s/actions/runs/36053395366), [commit run 36053389738](https://github.com/centerionware/not-k8s/actions/runs/36053389738) |
| 2026-09-24 | `3fb149bc59b56cddc3b5f962215c738ad26e3b7d` | K3s + Cilium migration | Source checks and target bootstrap passed; API import failed for 37 objects across cert-manager, Cilium, K3s Addon, and snapshot APIs that destination discovery did not expose. | [Run 36053700863](https://github.com/centerionware/not-k8s/actions/runs/36053700863) |
| 2026-09-24 | `3fb149bc59b56cddc3b5f962215c738ad26e3b7d` | Upstream Kubernetes + Cilium migration | Source checks and target bootstrap passed; API import failed for 26 objects across cert-manager, Cilium, snapshot, CertificateSigningRequest, and ClusterTrustBundle APIs unavailable in destination discovery or apply. | [Run 36053700863](https://github.com/centerionware/not-k8s/actions/runs/36053700863) |
| 2026-09-24 | `3fb149bc59b56cddc3b5f962215c738ad26e3b7d` | Five-node Docker preflight | Image build passed; first systemd container exited 255 before readiness. | [Run 36053700863](https://github.com/centerionware/not-k8s/actions/runs/36053700863) |
| 2026-09-24 | `99f232814f8a3ce2de2c71943eb4bf54a80a6930` | Focused nodemigrate checks | Crate tests and packaging passed; test includes grouped API restore failure reporting. | [Run 36055133206](https://github.com/centerionware/not-k8s/actions/runs/36055133206) |
| 2026-09-24 | `99f232814f8a3ce2de2c71943eb4bf54a80a6930` | PR shell validation and commit convention | Both passed. | [Shell run 36055133272](https://github.com/centerionware/not-k8s/actions/runs/36055133272), [commit run 36055128764](https://github.com/centerionware/not-k8s/actions/runs/36055128764) |
| 2026-09-24 | `99f232814f8a3ce2de2c71943eb4bf54a80a6930` | K3s + Cilium migration | Source checks and target bootstrap passed; API import failed for 37 objects across cert-manager, Cilium, K3s Addon, and snapshot APIs that destination discovery did not expose. The CRD count trace was not visible. | [Run 36055409069](https://github.com/centerionware/not-k8s/actions/runs/36055409069) |
| 2026-09-24 | `99f232814f8a3ce2de2c71943eb4bf54a80a6930` | Upstream Kubernetes + Cilium migration | Source checks and target bootstrap passed; API import failed for 26 objects across cert-manager, Cilium, and snapshot APIs, plus CertificateSigningRequest and ClusterTrustBundle application. The CRD count trace was not visible. | [Run 36055409069](https://github.com/centerionware/not-k8s/actions/runs/36055409069) |
| 2026-09-24 | `99f232814f8a3ce2de2c71943eb4bf54a80a6930` | Five-node Docker preflight | Image build passed; first systemd container exited 255 before readiness. | [Run 36055409069](https://github.com/centerionware/not-k8s/actions/runs/36055409069) |
| 2026-09-24 | `cc020c97bebef3ea9c4618723710df607ccd5bda` | Focused nodemigrate checks | Crate tests, packaging, shell validation, and commit convention passed. | [Tests 36056427012](https://github.com/centerionware/not-k8s/actions/runs/36056427012), [shell 36056427033](https://github.com/centerionware/not-k8s/actions/runs/36056427033), [commit 36056423711](https://github.com/centerionware/not-k8s/actions/runs/36056423711) |
| 2026-09-24 | `cc020c97bebef3ea9c4618723710df607ccd5bda` | K3s + Cilium migration | Source emitted the unattended warning and exported 506 objects including 48 CRDs. Target bootstrap passed; API import failed with 37 objects pending because destination discovery lacked cert-manager, Cilium, snapshot, and K3s Addon APIs. | [Run 36056682815](https://github.com/centerionware/not-k8s/actions/runs/36056682815) |
| 2026-09-24 | `cc020c97bebef3ea9c4618723710df607ccd5bda` | Upstream Kubernetes + Cilium migration | Source emitted the unattended warning and exported 488 objects including 44 CRDs. Target bootstrap passed; API import failed with 26 objects pending because destination discovery lacked cert-manager, Cilium, and snapshot APIs; CertificateSigningRequest and ClusterTrustBundle operations also failed. | [Run 36056682815](https://github.com/centerionware/not-k8s/actions/runs/36056682815) |
| 2026-09-24 | `cc020c97bebef3ea9c4618723710df607ccd5bda` | Five-node Docker preflight | Image build passed; first systemd container exited 255 before readiness. | [Run 36056682815](https://github.com/centerionware/not-k8s/actions/runs/36056682815) |
| — | — | Migration state and metadata fixtures | Integration script fingerprints all exported API objects, checks Node label/annotation/taint and ConfigMap annotation at every stage, and compares ConfigMap data hashes on return. The focused crate tests now pass. Migration round-trip and five-node runtime evidence remain pending. | — |
| 2026-09-24 | `336ea350502288d55bb6a57763494fa0aa89a475` | Focused quick-check `nodeapiserver,nodemigrate` | Passed, including the CRD update and ExtraValue codec changes. | [Run 36065050713](https://github.com/centerionware/not-k8s/actions/runs/36065050713) |
| 2026-09-24 | `336ea350502288d55bb6a57763494fa0aa89a475` | K3s+Cilium release-backed migration against `v0.8.0` | Source and target checks passed; utility migration completed and accepted 48 CRDs. CSI redeployment then reapplied migrated snapshot CRDs; v0.8.0 returned 404 before stage verification. Fixture follow-up will skip reapplying those migrated CRDs while still checking their APIs through redeployment. | [Run 36065059567](https://github.com/centerionware/not-k8s/actions/runs/36065059567) |
| 2026-09-24 | `336ea350502288d55bb6a57763494fa0aa89a475` | Upstream Kubernetes+Cilium release-backed migration against `v0.8.0` | Source and target checks passed; import failed for exactly two objects: CertificateSigningRequest due to the v0.8.0 protobuf ExtraValue codec and ClusterTrustBundle/v1 because the target does not serve that API. The current-branch fixes cannot alter the released target used by this run. | [Run 36065059567](https://github.com/centerionware/not-k8s/actions/runs/36065059567) |
| 2026-09-24 | `336ea350502288d55bb6a57763494fa0aa89a475` | Five-node Docker preflight | Image build passed; first systemd container exited 255 before readiness. | [Run 36065059567](https://github.com/centerionware/not-k8s/actions/runs/36065059567) |
| 2026-09-24 | `e6963be70467318f19b8c9fb8ef7197c0c9980b2` | K3s+Cilium release-backed migration against `v0.8.0` | Utility migration completed with 48 CRDs accepted. The migrated VolumeSnapshotClass remained usable without CRD reapplication. The target then denied `system:kube-controller-manager` pod creation (403), preventing CSI readiness, workload checks, and reverse migration. | [Run 36066311951](https://github.com/centerionware/not-k8s/actions/runs/36066311951) |
| 2026-09-24 | `e6963be70467318f19b8c9fb8ef7197c0c9980b2` | Upstream Kubernetes+Cilium release-backed migration against `v0.8.0` | Import again failed for exactly CertificateSigningRequest ExtraValue encoding and unsupported ClusterTrustBundle/v1. Source stayed disabled and the protected export was retained. | [Run 36066311951](https://github.com/centerionware/not-k8s/actions/runs/36066311951) |
| 2026-09-24 | `e6963be70467318f19b8c9fb8ef7197c0c9980b2` | Five-node Docker preflight | Image build passed; first systemd container exited 255 before readiness. | [Run 36066311951](https://github.com/centerionware/not-k8s/actions/runs/36066311951) |
| 2026-09-24 | `954f1be3d12fb7f0334db8dff6efae28ca0e4250` | Focused nodemigrate checks | API-version negotiation regression and complete nodemigrate crate checks passed. | [Run 36068373192](https://github.com/centerionware/not-k8s/actions/runs/36068373192) |
| 2026-09-24 | `954f1be3d12fb7f0334db8dff6efae28ca0e4250` | K3s+Cilium against `v0.8.0` | Utility build and release runtime verification passed. Forward migration reached target CSI readiness, where the v0.8.0 `system:kube-controller-manager` identity received 403 for pod creation; reverse migration was not reached. | [Run 36068417485](https://github.com/centerionware/not-k8s/actions/runs/36068417485) |
| 2026-09-24 | `954f1be3d12fb7f0334db8dff6efae28ca0e4250` | Upstream Kubernetes+Cilium against `v0.8.0` | Source checkpoint passed. Version fallback applied ClusterTrustBundle through v1beta1. Import then stopped on one CSR: the v0.8.0 server returned HTTP 500, `Decode(NotAnObject("io.k8s.api.certificates.v1.ExtraValue"))`. The source remained disabled and the protected export was retained. The current PR's nodeapiserver codec fix is not present in the released runtime. | [Run 36068417485](https://github.com/centerionware/not-k8s/actions/runs/36068417485) |
| 2026-09-24 | `954f1be3d12fb7f0334db8dff6efae28ca0e4250` | Five-node Docker preflight | Image build passed. The first privileged container started `/usr/lib/systemd/systemd`, then exited 255 before systemd readiness; no node-isolation assertions ran. | [Run 36068417485](https://github.com/centerionware/not-k8s/actions/runs/36068417485) |

| 2026-09-24 | `093f2e440b16c4a2a585b424b816b97ab49480a9` | Focused nodemigrate checks, shell validation, commit convention | Passed; crate tests include served-CRD-version and interactive/noninteractive risk-warning behavior. | [Checks 36058334362](https://github.com/centerionware/not-k8s/actions/runs/36058334362), [shell 36058334455](https://github.com/centerionware/not-k8s/actions/runs/36058334455), [commit 36058331222](https://github.com/centerionware/not-k8s/actions/runs/36058331222) |
| 2026-09-24 | `093f2e440b16c4a2a585b424b816b97ab49480a9` | K3s + Cilium migration against `v0.8.0` | Source setup and target bootstrap passed; warning appeared in log; 506 objects/48 CRDs exported. After 60 seconds, destination discovery still lacked all 51 served source CRD APIs; import failed with 37 objects pending. | [Run 36058570338](https://github.com/centerionware/not-k8s/actions/runs/36058570338) |
| 2026-09-24 | `093f2e440b16c4a2a585b424b816b97ab49480a9` | Upstream Kubernetes + Cilium migration against `v0.8.0` | Source setup and target bootstrap passed; warning appeared in log; 488 objects/44 CRDs exported. After 60 seconds, destination discovery still lacked all 47 served source CRD APIs; import failed with 26 objects pending. CertificateSigningRequest and ClusterTrustBundle operations also failed. | [Run 36058570338](https://github.com/centerionware/not-k8s/actions/runs/36058570338) |
| 2026-09-24 | `093f2e440b16c4a2a585b424b816b97ab49480a9` | Five-node Docker preflight | Image built; first systemd container exited 255 before readiness. | [Run 36058570338](https://github.com/centerionware/not-k8s/actions/runs/36058570338) |

| 2026-09-24 | `f9e4b31139a30fb7a453161e4dc2295983fcef8f` | Focused component checks | `nodemigrate` crate tests passed; quick-check passed for `nodeapiserver,nodecontroller,nodebootstrap`. | [Nodemigrate tests 36071161354](https://github.com/centerionware/not-k8s/actions/runs/36071161354), [quick-check 36071174298](https://github.com/centerionware/not-k8s/actions/runs/36071174298) |
| 2026-09-24 | `f9e4b31139a30fb7a453161e4dc2295983fcef8f` | K3s+Cilium against `v0.8.0` | Source checks and forward migration passed. CSI fixture redeployment then hit repeated 403s from the released `system:kube-controller-manager` identity creating ReplicaSets/Pods and reconciling EndpointSlices. This blocks workload verification and reverse migration; it is not evidence against the branch's updated RBAC. | [Run 36071174265](https://github.com/centerionware/not-k8s/actions/runs/36071174265), attempts 1 and 2 |
| 2026-09-24 | `f9e4b31139a30fb7a453161e4dc2295983fcef8f` | Upstream Kubernetes+Cilium against `v0.8.0` | Attempt 1 failed while fetching the runtime and did not enter migration. Attempt 2 reached the known CSR `ExtraValue` HTTP 500; the new rollback assertion passed: kubelet and source API recovered, and the protected export remained. | [Run 36071174265](https://github.com/centerionware/not-k8s/actions/runs/36071174265), attempt 2 |
| 2026-09-24 | `f9e4b31139a30fb7a453161e4dc2295983fcef8f` | Five-node Docker preflight | Image built, but the first privileged systemd container again exited 255 before readiness; no isolation assertions ran. | [Run 36071174265](https://github.com/centerionware/not-k8s/actions/runs/36071174265), attempt 2 |


| 2026-09-24 | `365247f64faf5e360a0eb4b3e758ac060d6bf307` | K3s+Cilium against `v0.8.0` | Source checks and forward migration passed; post-migration CSI/workload checks again received 403 from the old `system:kube-controller-manager` identity. | [Run 36073431846](https://github.com/centerionware/not-k8s/actions/runs/36073431846) |
| 2026-09-24 | `365247f64faf5e360a0eb4b3e758ac060d6bf307` | Upstream Kubernetes+Cilium against `v0.8.0` | The known CSR `ExtraValue` import error was returned; rollback passed, restoring kubelet and source API and retaining the protected export. | [Run 36073431846](https://github.com/centerionware/not-k8s/actions/runs/36073431846) |
| 2026-09-24 | `365247f64faf5e360a0eb4b3e758ac060d6bf307` | Five-node Docker preflight | Systemd reached readiness after using the private cgroup namespace and explicit `/run` modes. The next combined CRI/network/BTF/bpffs/storage check failed; diagnostics are being made specific. | [Run 36073431846](https://github.com/centerionware/not-k8s/actions/runs/36073431846) |

| 2026-09-25 | `b9c46f4ca11e80c03fe65be7c9626aabbd2d31cc` | K3s+Cilium against `v0.8.0` | Forward migration passed; the released runtime's `system:kube-controller-manager` then received 403s creating pods/ReplicaSets and reconciling EndpointSlices, so workload verification and reverse migration did not run. | [Run 36074502550](https://github.com/centerionware/not-k8s/actions/runs/36074502550) |
| 2026-09-25 | `b9c46f4ca11e80c03fe65be7c9626aabbd2d31cc` | Upstream Kubernetes+Cilium against `v0.8.0` | The expected released-runtime CSR `ExtraValue` import failure exercised rollback; source recovery and protected-export checks passed. | [Run 36074502550](https://github.com/centerionware/not-k8s/actions/runs/36074502550) |
| 2026-09-25 | `b9c46f4ca11e80c03fe65be7c9626aabbd2d31cc` | Five-node Docker preflight | The node image built and systemd reached readiness. CRI was healthy (`io.containerd.grpc.v1 cri - ok`), but the preflight falsely rejected its padded `ctr plugins ls` row. A field-based parser fix is pending CI validation. | [Run 36074502550](https://github.com/centerionware/not-k8s/actions/runs/36074502550) |
| 2026-09-25 | `09f09bde266a9b634ef93e4b12459aa6db8ec278` | PR validation | Shell syntax validation passed. | [Run 36075741102](https://github.com/centerionware/not-k8s/actions/runs/36075741102) |
| 2026-09-25 | `09f09bde266a9b634ef93e4b12459aa6db8ec278` | K3s+Cilium against `v0.8.0` | Utility build and forward migration passed; nodeapiserver readiness succeeded. The old runtime then returned 403s to `system:kube-controller-manager` while creating ReplicaSets, preventing hostPath CSI readiness and reverse migration. | [Run 36075750885](https://github.com/centerionware/not-k8s/actions/runs/36075750885) |
| 2026-09-25 | `09f09bde266a9b634ef93e4b12459aa6db8ec278` | Upstream Kubernetes+Cilium against `v0.8.0` | Utility build passed; the known CSR `ExtraValue` API import failure exercised rollback, which restored the source services and retained the protected export. | [Run 36075750885](https://github.com/centerionware/not-k8s/actions/runs/36075750885) |
| 2026-09-25 | `09f09bde266a9b634ef93e4b12459aa6db8ec278` | Five-node Docker preflight | Image build, systemd readiness, CRI check, network namespace creation, BTF discovery, and bpffs mount passed. The probe then failed to compile its BPF program because clang rejected a global `license` symbol. The symbol was renamed to `LICENSE`; preflight rerun pending. | [Run 36075750885](https://github.com/centerionware/not-k8s/actions/runs/36075750885) |
| 2026-09-25 | `cb392851f7969c6c13e311733eac9b9293696fdc` | PR validation | Workflow shell/path validation passed; migration jobs were skipped because this was a push-triggered PR check. | [Run 36077003195](https://github.com/centerionware/not-k8s/actions/runs/36077003195) |
| 2026-09-25 | `cb392851f7969c6c13e311733eac9b9293696fdc` | Docker-only five-node preflight | Dispatch skipped the K3s and upstream lanes as requested. Image build, systemd, CRI, network namespace, BTF discovery, bpffs mount, and clang BPF compilation passed. Loading failed because Ubuntu's `bpftool` wrapper had no executable matching the runner's `6.17.0-1022-azure` kernel. The image now installs and selects a generic versioned bpftool directly; focused rerun pending. | [Run 36077006621](https://github.com/centerionware/not-k8s/actions/runs/36077006621) |
| 2026-09-25 | `3c1c75ae5b6cba74c39c6040ace131ef052d2124` | Docker-only image build | Failed before probes because the versioned `bpftool` binary was packaged directly under `/usr/lib/linux-tools-*`, while the initial Dockerfile search assumed an intermediate `/usr/lib/linux-tools/` directory. The path search was widened to the observed Ubuntu package layout. | [Run 36077228915](https://github.com/centerionware/not-k8s/actions/runs/36077228915) |
| 2026-09-25 | `dd31443360d6d3412783ebe27d7b44661f496eef` | Docker-only five-node preflight | Passed. Built the image; all five isolated systemd containers passed CRI, BTF/bpffs, BPF compile/load, distinct network and mount namespaces and machine IDs, independent writable persistent volumes, pairwise inter-node reachability, and single-node stop/restart isolation. K3s and upstream migration lanes were intentionally skipped. This verifies container isolation only, not Kubernetes/Cilium behavior or migration parity. | [Run 36077448685](https://github.com/centerionware/not-k8s/actions/runs/36077448685) |
| 2026-09-25 | `dd31443360d6d3412783ebe27d7b44661f496eef` | PR validation | Shell and snapshot checks passed. | [Run 36077436572](https://github.com/centerionware/not-k8s/actions/runs/36077436572) |
| 2026-09-25 | `459570ee4fdcea8128456f84058882159111a85a` | Focused nodemigrate quick-check | Passed the nodemigrate component check, including K3s container runtime endpoint detection and source sandbox shutdown ordering. | [Run 36104107511](https://github.com/centerionware/not-k8s/actions/runs/36104107511) |
| 2026-09-25 | `459570ee4fdcea8128456f84058882159111a85a` | Branch-runtime K3s+Cilium and upstream+Cilium | Both utility/runtime builds and Docker five-node preflight passed. K3s forward import accepted 48 CRDs and target API readiness passed, but Cilium/CNI did not initialize, blocking CoreDNS, CSI readiness, post-cutover workload checks, and reverse migration. Upstream import failed on the CertificateRequest admission webhook with HTTP 500; source API and protected export recovery passed. | [Run 36104132045](https://github.com/centerionware/not-k8s/actions/runs/36104132045) |
| 2026-09-25 | `459570ee4fdcea8128456f84058882159111a85a` | Latest-release `v0.8.0` K3s+Cilium and upstream+Cilium | Utility builds, `v0.8.0` digest verification, and Docker preflight passed. K3s forward import/API readiness passed, then failed Cilium/CNI and CSI readiness. Upstream failed on the unavailable CertificateRequest webhook and the known CSR `ExtraValue` protobuf defect; rollback restored source API and retained the protected export. No reverse migration or semantic parity gate passed. | [Run 36104248971](https://github.com/centerionware/not-k8s/actions/runs/36104248971) |
| 2026-09-25 | `1a6f43162dae86349534129bae0217f34a36c398` | Branch-runtime K3s+Cilium and upstream+Cilium diagnostics | Both utility/runtime builds and Docker preflight passed. K3s again imported 48 CRDs and passed API readiness, then failed Cilium/CNI/CSI readiness. Process ancestry and cgroup diagnostics confirmed the socket-owning Envoy was still in source K3s CRI container `ef96e5cf937fed840c1bfcc03df0ef667927c7f666ba4963da35faaa9f80f39a`, parented by the source K3s containerd shim. Upstream again failed CertificateRequest webhook admission and recovered its source API/export. Artifacts: `/tmp/nodemigrate-36107163563/nodemigrate-k3s-36107163563/nodemigrate-k3s.log` and `/tmp/nodemigrate-36107163563/nodemigrate-kubernetes-36107163563/nodemigrate-kubernetes.log`. No workload/return-state parity checkpoint passed. | [Run 36107163563](https://github.com/centerionware/not-k8s/actions/runs/36107163563) |
| 2026-09-25 | `5736a03302b203e49cea66365f54b55d53e293a5` | Initial focused nodemigrate validation | `quick-check` and both migration workflow lanes failed while compiling `nodemigrate`: the process matcher used `HashSet<&String>::contains(&str)`. Docker five-node preflight passed in both integration runs; neither runtime migration started. Fixed by storing borrowed string slices and comparing IDs exactly; reruns pending. | [Quick-check 36109695038](https://github.com/centerionware/not-k8s/actions/runs/36109695038), [branch migration 36109695161](https://github.com/centerionware/not-k8s/actions/runs/36109695161), [v0.8.0 migration 36109695193](https://github.com/centerionware/not-k8s/actions/runs/36109695193) |
| 2026-09-25 | `e82ff368719e030e881d503ed5b3e915cdb81496` | Focused nodemigrate quick-check | Passed after correcting the borrowed `HashSet` key type. | [Run 36110023200](https://github.com/centerionware/not-k8s/actions/runs/36110023200) |
| 2026-09-25 | `e82ff368719e030e881d503ed5b3e915cdb81496` | Branch-runtime K3s+Cilium and upstream+Cilium | K3s source Envoy cleanup succeeded and stopped 2 processes by source CRI ID; forward import accepted all 48 CRDs and target API became ready. The target Envoy then could not connect to Cilium agent's missing `xds.sock`, CNI/CoreDNS stayed unready, and hostPath CSI could not schedule; reverse migration and parity checks did not run. Upstream again failed CertificateRequest webhook admission, rolled back, and retained its export. Logs: `/tmp/nodemigrate-36110023502/nodemigrate-k3s-36110023502/nodemigrate-k3s.log` and `/tmp/nodemigrate-36110023502/nodemigrate-kubernetes-36110023502/nodemigrate-kubernetes.log`. | [Run 36110023502](https://github.com/centerionware/not-k8s/actions/runs/36110023502) |
| 2026-09-25 | `e82ff368719e030e881d503ed5b3e915cdb81496` | Latest-release `v0.8.0` K3s+Cilium and upstream+Cilium | K3s forward import/API readiness passed, then Cilium/CSI readiness failed; diagnostics showed source Envoy still held the host socket, with a cgroup container ID different from its parent shim's CRI `-id`, exposing a gap in cgroup-only identity matching. Upstream failed on the unavailable CertificateRequest webhook and known CSR `ExtraValue` protobuf bug; rollback restored source API and retained export. Logs: `/tmp/nodemigrate-36110023663/nodemigrate-k3s-36110023663/nodemigrate-k3s.log` and `/tmp/nodemigrate-36110023663/nodemigrate-kubernetes-36110023663/nodemigrate-kubernetes.log`. No round trip or parity checkpoint passed. | [Run 36110023663](https://github.com/centerionware/not-k8s/actions/runs/36110023663) |
| 2026-09-25 | `aa4580f0938fb8c23b816c41b61d5e96c8314e4a` | Focused nodemigrate quick-check | Compiled successfully; 64 tests passed and one fixture assertion failed because the negative CRI example was named `cilium-agent`. Corrected that fixture to `cilium-operator`; the rerun at `1faaf0634d89a6a116ce28d5a93374e6d082d54f` passed all nodemigrate unit tests. | [Run 36112039291](https://github.com/centerionware/not-k8s/actions/runs/36112039291), [corrected run 36112668805](https://github.com/centerionware/not-k8s/actions/runs/36112668805) |
| 2026-09-25 | `aa4580f0938fb8c23b816c41b61d5e96c8314e4a` | Branch-runtime K3s+Cilium and upstream+Cilium | K3s source cleanup recorded three Cilium CRI IDs, stopped 2 orphan Envoy processes by source container identity, removed 3 stale sockets, and completed forward object import/API readiness. Target Envoy still logged `xds.sock` missing during startup; CNI/CoreDNS and hostPath CSI readiness failed, so no workload or reverse-migration checkpoint passed. Upstream hit the unavailable CertificateRequest webhook, rolled back, and retained its protected export. Logs: `/tmp/nodemigrate-36112039343/nodemigrate-k3s-36112039343/nodemigrate-k3s.log` and `/tmp/nodemigrate-36112039343/nodemigrate-kubernetes-36112039343/nodemigrate-kubernetes.log`. | [Run 36112039343](https://github.com/centerionware/not-k8s/actions/runs/36112039343) |
| 2026-09-25 | `aa4580f0938fb8c23b816c41b61d5e96c8314e4a` | Latest-release `v0.8.0` K3s+Cilium and upstream+Cilium | K3s also stopped 2 source Envoy processes by recorded Cilium CRI identity, removed 3 stale sockets, and completed forward API import/readiness; target Cilium/CSI readiness still failed. Upstream failed the CertificateRequest webhook and the known released CSR `ExtraValue` protobuf path; rollback restored source API and retained export. No reverse migration or state parity checkpoint passed. Logs: `/tmp/nodemigrate-36112039214/nodemigrate-k3s-36112039214/nodemigrate-k3s.log` and `/tmp/nodemigrate-36112039214/nodemigrate-kubernetes-36112039214/nodemigrate-kubernetes.log`. | [Run 36112039214](https://github.com/centerionware/not-k8s/actions/runs/36112039214) |
| 2026-09-25 | `e42a5d4b1dcad6ce4a1e132c575a36f7a590fb9a` | PR migration workflow validation | Shell syntax and migration snapshot checks passed. Runtime follow-up revealed the new collector selected the stale source kubeconfig after cutover, so it emitted no Cilium agent logs; the collector is being changed to use the active `KUBECONFIG`. | [Run 36114345183](https://github.com/centerionware/not-k8s/actions/runs/36114345183) |
| 2026-09-25 | `e42a5d4b1dcad6ce4a1e132c575a36f7a590fb9a` | Branch-runtime migration with expanded Cilium diagnostics | K3s stopped 2 source Envoy processes, removed 3 stale sockets, and passed forward API import/readiness; target Cilium/CSI did not become ready. Upstream failed the CertificateRequest webhook and recovered source/API export. The log collector did not capture Cilium agent output because it queried with the stale source kubeconfig. Logs: `/tmp/nodemigrate-36114368998/nodemigrate-k3s-36114368998/nodemigrate-k3s.log` and `/tmp/nodemigrate-36114368998/nodemigrate-kubernetes-36114368998/nodemigrate-kubernetes.log`. Active-kubeconfig correction and rerun pending. | [Run 36114368998](https://github.com/centerionware/not-k8s/actions/runs/36114368998) |
| 2026-09-25 | `ccb4d413ae61418dd3c9121e02992781f874cacd` | Branch-runtime migration with active-kubeconfig diagnostics | Nodemigrate and runtime builds plus five-node Docker isolation preflight passed. Both migration lanes failed: K3s forward import and API readiness passed, source Envoy cleanup stopped tracked processes and removed stale sockets, but target Cilium remained `PodInitializing`; the selected agent Pod lookup returned `NotFound`, its init log only reached the API-server connection, and CoreDNS sandbox creation failed with `cni plugin not initialized`. Upstream failed importing `CertificateRequest migration-test-1` with HTTP 500; source recovery and protected export retention passed. No return migration or parity checkpoint passed. Logs: `/tmp/nodemigrate-36116466142/nodemigrate-k3s-36116466142/nodemigrate-k3s.log` and `/tmp/nodemigrate-36116466142/nodemigrate-kubernetes-36116466142/nodemigrate-kubernetes.log`. | [Run 36116466142](https://github.com/centerionware/not-k8s/actions/runs/36116466142) |
| 2026-09-25 | `4b3301af7713a90b4273275415c94e2164a1f5d2` | Branch-runtime K3s+Cilium and upstream+Cilium | Nodemigrate/runtime builds and five-node Docker preflight passed. K3s source checks passed; forward import accepted all 48 CRDs and destination API readiness passed. The Cilium config init container connected to `https://10.1.0.220:6443` and read `cilium-config`, but target CNI stayed uninitialized and hostPath CSI setup failed before the nodestore checkpoint. The failure handler still used the stopped source kubeconfig, so its unknown-CA errors and missing selected Pod did not diagnose destination state; this is fixed in the current worktree. Upstream again failed applying `CertificateRequest migration-test-1` with HTTP 500 because the cert-manager webhook could not be reached while target CNI failed; source recovery and protected-export retention passed. No reverse migration or parity checkpoint passed. Logs: `/tmp/nodemigrate-36119449016/nodemigrate-k3s-36119449016/nodemigrate-k3s.log` and `/tmp/nodemigrate-36119449016/nodemigrate-kubernetes-36119449016/nodemigrate-kubernetes.log`. | [Run 36119449016](https://github.com/centerionware/not-k8s/actions/runs/36119449016) |
| 2026-09-25 | `8230c5f63faabada5393cc29cd3c6aed082177cb` | Focused `nodebootstrap` quick-check | Passed, including the API serving-certificate SAN regression test; the branch-runtime Cilium readiness gate remains unverified. | [Run 36120097438](https://github.com/centerionware/not-k8s/actions/runs/36120097438) |
| 2026-09-25 | `d739633a15a330565339b7028e9339e73326b207` | K3s+Cilium branch-runtime migration | Source fixture passed; forward import accepted all 48 CRDs and target API readiness passed. Target Cilium Envoy then crashed with `errno=98`, CNI stayed uninitialized, and CSI setup failed. Diagnostics were on the destination kubeconfig; Envoy's process/container/shim identities did not agree, so ownership cause remains unresolved. No target checkpoint, reverse migration, or parity result. | [Run 36121960626](https://github.com/centerionware/not-k8s/actions/runs/36121960626) |
| 2026-09-25 | `d739633a15a330565339b7028e9339e73326b207` | Upstream Kubernetes+Cilium branch-runtime migration | Import failed when CertificateRequest admission called the unreachable cert-manager webhook while destination Cilium/CNI was unavailable. Source rollback and protected-export retention passed. No target checkpoint or reverse migration. | [Run 36121960626](https://github.com/centerionware/not-k8s/actions/runs/36121960626) |
| 2026-09-25 | `217bf87275f1abbacdcb1f6e587b5914f72f3f92` | Expanded migration fixture | Added binary ConfigMap/Secret, real-token RBAC allow/deny checks, Job/CronJob, all-node DaemonSet, CSI StatefulSet claim template, Helm release inspection/dry-run, and Gateway API routing assertions. `bash -n`, snapshot-filter check, and `git diff --check` passed. Runtime validation is pending. | Dedicated migration workflow pending |
| 2026-09-25 | `217bf87275f1abbacdcb1f6e587b5914f72f3f92` | Branch-runtime migration and Docker preflight | `nodemigrate` and combined runtime builds passed; five-node Docker preflight passed. Both migration lanes failed during source fixture setup before nodemigrate was invoked: the GatewayClass became Accepted but its Gateway did not become Programmed. Cilium, CSI, cert-manager and workload setup were healthy at the failure checkpoint. Traefik was Running, but its logs and Gateway status were not captured. | [Run 36124385324](https://github.com/centerionware/not-k8s/actions/runs/36124385324); logs `/tmp/nodemigrate-36124385324/` |
| 2026-09-25 | `404d68f7b3aa6dc04192e2061e48f563ae2d5589` | Expanded migration fixture follow-up | Adds ClusterRole/ClusterRoleBinding Node-read authorization, ResourceQuota, LimitRange, PriorityClass scheduling state, live ConfigMap/Secret consumption, and Cilium Helm release/value/manifest checks plus a pinned server-side dry-run upgrade. It also waits for Traefik rollout before creating Gateway resources and captures Gateway status/Traefik logs on failure. Shell syntax, snapshot-filter, and whitespace checks passed. Runtime validation is pending. | Dedicated migration workflow pending |
| 2026-09-25 | `7899ac9e1d2d6140d4b799bf64beed1657ac6c1c` | Branch-runtime migration and five-node Docker preflight | Utility/runtime builds and Docker isolation preflight passed. Both source lanes failed before nodemigrate ran while waiting for `migration-seed`: upstream Kubernetes events later reported `Insufficient cpu` for the fixture workloads, and K3s also left its workload Pods Pending. The run captured that the Traefik Gateway listener on port 80 was rejected with `PortUnavailable` because no matching entryPoint existed; GatewayClass and Traefik Pod were healthy. No migration stage or resource parity assertion ran. | [Run 36126850738](https://github.com/centerionware/not-k8s/actions/runs/36126850738); logs `/tmp/nodemigrate-36126850738/` |
| 2026-09-25 | `f2d2c7639b78ee1a1cb9b6609903e6374fb24716` | Branch-runtime migration and five-node Docker preflight | Utility/runtime builds and Docker preflight passed. Fixture resource requests let the source PVCs, seed Pod, and nginx Deployment become ready; the port 8080 Gateway became Programmed. Both lanes then timed out using `kubectl wait` on HTTPRoute conditions. The upstream route status shows `Accepted=True` and `ResolvedRefs=True` nested under `status.parents[].conditions`; no nodemigrate command ran. | [Run 36129138298](https://github.com/centerionware/not-k8s/actions/runs/36129138298); logs `/tmp/nodemigrate-36129138298/` |
| 2026-09-25 | `b67692295b72763f5706d59774bda7d6527f51eb` | HTTPRoute condition wait correction | The Gateway and seed/workload setup passed, and both lanes got past the nested HTTPRoute condition poll. Source fixture validation then failed at the PriorityClass check because `$stage` was unset under `set -u`; neither lane invoked nodemigrate. Docker preflight and utility/runtime builds passed. | [Run 36130714079](https://github.com/centerionware/not-k8s/actions/runs/36130714079); logs `/tmp/nodemigrate-36130714079/` |
| 2026-09-25 | `4c05f5c73b9fb77e65783540e6766fd2ec56a544` | Source stage initialization | Both fixture lanes initialized and ran source checks, then failed the PriorityClass assertion because the API omitted the default `globalDefault: false` field. No nodemigrate command ran. Docker preflight and utility/runtime builds passed. | [Run 36131837328](https://github.com/centerionware/not-k8s/actions/runs/36131837328); logs `/tmp/nodemigrate-36131837328/` |
| 2026-09-25 | `89ffb03f46f29dec02b9fb3cff5cf22a3c127013` | PriorityClass default handling | Both source lanes passed fixture install, Gateway API, PriorityClass, and other workload checks. They then failed because jq rejected `.data.migration-secret` as invalid syntax for a hyphenated key; neither lane invoked nodemigrate. Utility/runtime builds and Docker preflight passed. | [Run 36133100159](https://github.com/centerionware/not-k8s/actions/runs/36133100159); logs `/tmp/nodemigrate-36133100159/` |
| 2026-09-25 | `de7d79333131c566dd1260e9cdcbcf159d8a33a4` | Secret jq key selection | Both source lanes passed fixture setup, ConfigMap/Secret preservation checks, StatefulSet storage, Job, and CronJob checks. They then failed because an unquoted heredoc evaluated `$(NODE_NAME)` on the runner; the RBAC Node-read Job timed out. Utility/runtime builds and Docker preflight passed. | [Run 36134496060](https://github.com/centerionware/not-k8s/actions/runs/36134496060); logs `/tmp/nodemigrate-36134496060/` |
| 2026-09-25 | `b48511d3c393ecdc9e1c0126210267d62946d266` | RBAC Node-read Job manifest | The heredoc substitution was corrected. Both source lanes then left all three RBAC Pods Pending with `Insufficient cpu`; no migration command ran. Utility/runtime builds and Docker preflight passed. | [Run 36136419993](https://github.com/centerionware/not-k8s/actions/runs/36136419993); logs `/tmp/nodemigrate-36136419993/` |
| Follow-up to `b48511d3c393ecdc9e1c0126210267d62946d266` | RBAC and data-check Pod requests | Add explicit 1m CPU/1Mi memory requests to helper probe and PV data-check containers. Shell/snapshot/diff checks and migration rerun pending. | Not dispatched yet |
| 2026-09-25 | `87be29a496c073d899c70517e378dde785988925` | Branch-runtime K3s+Cilium and upstream+Cilium | Both lanes passed utility/runtime builds, Docker preflight, source workload/storage/cert/Helm/RBAC fixtures, and accepted/programmed Gateway conditions. Ingress returned HTTP 200 with nginx; the Gateway request returned HTTP 404 because the probe used Service port 80→8000 instead of Gateway listener port 8080. Neither lane invoked nodemigrate. | [Run 36140783446](https://github.com/centerionware/not-k8s/actions/runs/36140783446); logs `/tmp/nodemigrate-36140783446/artifacts/` |
| Follow-up to `87be29a496c073d899c70517e378dde785988925` | Gateway fixture probe endpoint | Probe Ingress over Service port 80 and Gateway API over Traefik's configured Pod listener port 8080. HTTP status/body and endpoint diagnostics isolated the mismatch. Local shell/snapshot/diff checks and runtime rerun pending. | Not dispatched yet |
| 2026-09-25 | `36fb2e70852ef4711db60128d29c483b115e41dc` | Branch-runtime K3s+Cilium and upstream+Cilium | Both source fixtures passed all checks, including Ingress/Gateway traffic, and both invoked nodemigrate. Destination import failed because the Gateway API safe-upgrades policy called unregistered global CEL `matches`, rejecting three CRDs. Upstream also hit a CertificateRequest HTTP 500. Rollback restored the source API and retained protected exports in both lanes. Utility/runtime builds and Docker preflight passed. | [Run 36142617576](https://github.com/centerionware/not-k8s/actions/runs/36142617576); logs `/tmp/nodemigrate-36142617576/artifacts/` |
| 2026-09-25 | `c56d3f3ecce3a43339ba0df23aec02a36193d841` | `nodeapiserver` quick-check | The CEL global `matches(string, regex)` regression passed with the focused nodeapiserver quick-check. | [Run 36145507191](https://github.com/centerionware/not-k8s/actions/runs/36145507191) |
| 2026-09-25 | `c56d3f3ecce3a43339ba0df23aec02a36193d841` | Branch-runtime K3s+Cilium and upstream+Cilium | Utility/runtime builds, Docker five-node preflight, and all source fixtures passed in both lanes. Both invoked migration. The global `matches` issue no longer appeared. Both imports failed on three Gateway CRDs because valid bounded map-key CEL rules were treated as generic objects and their key regex costs saturated. Upstream also failed importing CertificateRequest because destination Cilium networking left the cert-manager webhook unreachable. Both lanes rolled back and retained protected exports; no target, reverse, or parity checkpoint passed. | [Run 36145520889](https://github.com/centerionware/not-k8s/actions/runs/36145520889); artifacts `/tmp/nodemigrate-36145520889/artifacts/` |
| 2026-09-25 | `6a2e99d6b205626ae125e231d276c1936c3f1986` | `nodeapiserver` focused quick-check | Compilation failed only in the new `path.rs` unit tests: their `decl_type` module path was one level too shallow. The production source compiled for the branch migration workflow, which is still running. The test references are corrected in the next commit; rerun pending. | [Run 36148371547](https://github.com/centerionware/not-k8s/actions/runs/36148371547) |
| 2026-09-25 | `8555845f65bd0faed72d21d9b0b8c8b547960e6c` | `nodeapiserver` focused quick-check | The test-only module path correction passed the focused nodeapiserver unit-test job. | [Run 36148848411](https://github.com/centerionware/not-k8s/actions/runs/36148848411) |
| 2026-09-25 | `f97432ccc3edc65c846cb4c3c7bc7da3e30487f9` | Branch-runtime K3s+Cilium and upstream+Cilium | Utility/runtime builds, five-node Docker preflight, and source fixtures passed. All CRD apply requests passed: 58/58 in K3s and 54/54 upstream, confirming the CEL global function, typed map-key comprehensions, and bounded substring fixes. K3s forward migration completed and destination API readiness passed, but Cilium Envoy then failed binding its host socket (`errno=98`), CNI stayed uninitialized, and hostPath CSI/CoreDNS could not recover; no target workload checkpoint, reverse migration, or parity comparison passed. Upstream import failed on CertificateRequest admission because cert-manager's webhook was unreachable while destination CNI was unavailable; source rollback and protected-export retention passed. | [Run 36150670405](https://github.com/centerionware/not-k8s/actions/runs/36150670405); artifacts `/tmp/nodemigrate-36150670405/artifacts/` |
| Follow-up to `6a2e99d6b205626ae125e231d276c1936c3f1986` | nodeapiserver bounded substring cost | Propagate the source string maximum to `substring` result cost and add a TLSRoute-style bounded hostname regression. Focused quick-check passed, and the branch runtime accepted every source CRD in both lanes. Target Cilium startup and the upstream cert-manager webhook remain blocked by CNI networking. | Tests [36150653684](https://github.com/centerionware/not-k8s/actions/runs/36150653684); migration [36150670405](https://github.com/centerionware/not-k8s/actions/runs/36150670405) |
| 2026-09-25 | `f97432ccc3edc65c846cb4c3c7bc7da3e30487f9` | `nodeapiserver` focused quick-check | The bounded `substring` result-cost regression and the rest of the focused nodeapiserver unit tests passed. | [Run 36150653684](https://github.com/centerionware/not-k8s/actions/runs/36150653684) |
| 2026-09-25 | `a35ab9ba8cba149613381a5a3738879205ce31fe` | K3s+Cilium migration using latest regular release `v0.8.0` | Source K3s `v1.35.0+k3s1` passed the expanded fixture and emitted the unattended risk warning; nodemigrate captured 571 API objects/58 CRDs and protected the export. The v0.8.0 destination accepted 55 CRD requests and rejected the three Gateway API CRDs because its CEL evaluator lacks global `matches`; Gateway API v1 could not then be served. Source rollback and export retention passed. No destination checkpoint, reverse migration, or parity comparison ran. | [Run 36153673997](https://github.com/centerionware/not-k8s/actions/runs/36153673997); log `/tmp/nodemigrate-36153673997/artifacts/nodemigrate-k3s-36153673997/nodemigrate-k3s.log` |
| 2026-09-25 | `a35ab9ba8cba149613381a5a3738879205ce31fe` | Upstream Kubernetes+Cilium migration using latest regular release `v0.8.0` | Upstream Kubernetes `v1.37.1` passed the expanded fixture and emitted the unattended risk warning; nodemigrate captured 551 API objects/54 CRDs and protected the export. The v0.8.0 destination accepted 51 CRD requests and rejected the three Gateway API CRDs for missing CEL `matches`; CertificateRequest admission failed with HTTP 500 while its webhook was unreachable, and CSR import also returned HTTP 500 (consistent with the separately tracked v0.8.0 `ExtraValue` codec defect). Source rollback and export retention passed. No destination checkpoint, reverse migration, or parity comparison ran. | [Run 36153673997](https://github.com/centerionware/not-k8s/actions/runs/36153673997); log `/tmp/nodemigrate-36153673997/artifacts/nodemigrate-kubernetes-36153673997/nodemigrate-kubernetes.log` |
| 2026-09-25 | `a35ab9ba8cba149613381a5a3738879205ce31fe` | Shared migration CI infrastructure | Five-node Docker preflight passed. Utility build passed in both lanes; the normal build gate and general e2e were not run. The dedicated migration jobs failed at API object import, intentionally preventing later target, reverse, and resource-parity checks. | [Run 36153673997](https://github.com/centerionware/not-k8s/actions/runs/36153673997) |
| 2026-09-25 | `85ed67ed2d4b5540b9cbe3268b061955d946a98b` | K3s+Cilium migration using latest regular release `v0.8.0` | K3s `v1.35.0+k3s1` passed source checks and nodemigrate captured 571 objects/58 CRDs. The utility selected `/etc/cni/net.d` and found `05-cilium.conflist`; migration then failed when the v0.8.0 destination rejected three Gateway API CRDs because CEL `matches` is unsupported. Rollback restored K3s and retained the protected export. Target workload, reverse, and parity checks did not run. | [Run 36174946764](https://github.com/centerionware/not-k8s/actions/runs/36174946764); log `/tmp/nodemigrate-36174946764/k3s.log` |
| 2026-09-25 | `85ed67ed2d4b5540b9cbe3268b061955d946a98b` | Upstream Kubernetes+Cilium migration using latest regular release `v0.8.0` | Kubernetes `v1.37.1` passed source checks and nodemigrate captured 551 objects/54 CRDs. The released destination rejected three Gateway API CRDs for missing CEL `matches`; CertificateRequest admission returned HTTP 500 while the webhook was unreachable, and CSR import also returned HTTP 500. Rollback restored the source and retained the protected export. Target workload, reverse, and parity checks did not run. | [Run 36174946764](https://github.com/centerionware/not-k8s/actions/runs/36174946764); log `/tmp/nodemigrate-36174946764/kubernetes.log` |
| 2026-09-25 | `85ed67ed2d4b5540b9cbe3268b061955d946a98b` | Five-node Docker preflight and migration CI scope | Docker five-node isolation preflight passed. The standalone nodemigrate build passed in both lanes. The dedicated migration workflow failed during API import; general build and full e2e gates were not run. | [Run 36174946764](https://github.com/centerionware/not-k8s/actions/runs/36174946764) |

For every new result, record the commit SHA, workflow run URL, lane, resolved
Kubernetes/K3s/Cilium/add-on versions, and pass/fail state at each checkpoint.
Keep failures and skipped checks visible rather than replacing them with a
later green run.


## 2026-09-27 follow-up

| SHA | Workflow | Result | Evidence |
| --- | --- | --- | --- |
| `de2b9ed91210a4894bcdb0203bfd9320109335a4` | `nodeapiserver,nodescheduler` quick-check | Failed one new API pruning test; 1,423 tests passed. The test found that pruning ignored properties nested under OpenAPI `allOf`. This regression is fixed in `1dc1c2db`. | [Quick-check 36309717348](https://github.com/centerionware/not-k8s/actions/runs/36309717348); saved log `/tmp/nodemigrate-36309717348/quick-check.log` |
| `de2b9ed91210a4894bcdb0203bfd9320109335a4` | Branch-runtime K3s+Cilium and upstream+Cilium migration | Both lanes passed source StatefulSet PVC/PV/data checks, reached nodestore verification, then failed because the newly registered hostpath `CSINode` had no Node owner reference. Neither lane completed nodestore storage checks or the return leg. This SHA predates the allOf pruning fix. | [Migration 36309717243](https://github.com/centerionware/not-k8s/actions/runs/36309717243); logs `/tmp/nodemigrate-36309717243/artifacts/` |
| `de2b9ed91210a4894bcdb0203bfd9320109335a4` | Latest regular release (`v0.8.0`) K3s+Cilium and upstream+Cilium migration | Running against the exact latest regular runtime to check the current utility against the user's required 0.8.0 baseline. | [Migration 36310054614](https://github.com/centerionware/not-k8s/actions/runs/36310054614) |
| `1dc1c2db1efec37bb1e5cab08f78414388e16d1a` | `nodeapiserver,nodescheduler` quick-check | Passed. The allOf pruning fix and CSIDriver schema/protobuf/scheduler regressions passed. | [Quick-check 36310603471](https://github.com/centerionware/not-k8s/actions/runs/36310603471) |
| `1dc1c2db1efec37bb1e5cab08f78414388e16d1a` | Branch-runtime K3s+Cilium and upstream+Cilium migration | Both lanes reached nodestore and failed the CSINode owner-reference assertion: hostPath driver was registered but the CSINode had no owner reference. Neither lane ran nodestore PV/PVC/data assertions or return migration. No PV deletion was reported. Target diagnostics also logged the hostPath CSI provider returning `NotFound` for the StatefulSet's existing volume handle; this remains a separate storage issue to verify after the ownership fix. | [Migration 36310603619](https://github.com/centerionware/not-k8s/actions/runs/36310603619); artifacts `/tmp/nodemigrate-36310603619/artifacts/` |
| `42162ef20afca2e4b4f616cc1d83fbd05071a9ae` | `nodelet` quick-check | Passed, including the CSINode current-Node owner-reference repair. | [Quick-check 36311005893](https://github.com/centerionware/not-k8s/actions/runs/36311005893) |
| `42162ef20afca2e4b4f616cc1d83fbd05071a9ae` | Branch-runtime K3s+Cilium and upstream+Cilium migration | Five-node Docker preflight passed; upstream lane has entered migration and K3s is still building. This is the first migration run containing the Nodelet repair. | [Migration 36311005956](https://github.com/centerionware/not-k8s/actions/runs/36311005956) |
| `de2b9ed91210a4894bcdb0203bfd9320109335a4` | Latest regular release (`v0.8.0`) K3s+Cilium and upstream+Cilium migration | Run is terminal failure. K3s rejected Gateway API CRDs due CEL rule-cost overflow; upstream import rejected Gateway CRDs and returned HTTP 500 for CertificateRequest and CSR. Neither lane completed nodestore semantic checks or return migration; source PV/PVC/data checks passed and no PV deletion was reported. | [Migration 36310054614](https://github.com/centerionware/not-k8s/actions/runs/36310054614); K3s log `/tmp/nodemigrate-36310054614/artifacts/nodemigrate-k3s.log`; upstream log `/tmp/nodemigrate-36310054614/upstream/nodemigrate-kubernetes.log` |
| `4c319d08fccad2e506441802e929cd398e0bc764` | Branch-runtime K3s+Cilium and upstream+Cilium round trip | Preflight passed. Upstream failed forward Cilium readiness after `EROFS` on Cilium host sysctls and restored the source. K3s passed source and nodestore storage/parity checks and began return migration; job timed out at 180 minutes before returned-source verification. No PV deletion was reported. | [Migration 36321804254](https://github.com/centerionware/not-k8s/actions/runs/36321804254); logs `/tmp/nodemigrate-36321804254-upstream/nodemigrate-kubernetes.log` and `/tmp/nodemigrate-36321804254-k3s/nodemigrate-k3s.log` |
| `5ce3f3d613ec6caa08623e542ad79984c42c9bbc` | `nodemigrate` crate checks and migration-workflow validation | Focused crate tests passed; static migration validation passed. General build and migration runtime jobs were skipped for the PR event. | [Nodemigrate checks 36334107161](https://github.com/centerionware/not-k8s/actions/runs/36334107161); [workflow validation 36334107153](https://github.com/centerionware/not-k8s/actions/runs/36334107153) |
| `012f1b7351da10cd78bec309e1efc6a2e9de439e` | Latest regular runtime, K3s+Cilium and upstream+Cilium | Run terminal failure after both source fixtures passed. Both lanes' source PV/PVC/data checks passed and protected exports were retained; no PV deletion was reported. Release runtime rejected Gateway API CRDs on CEL rule-cost overflow/type errors, preventing nodestore checks and return migration. Upstream also returned HTTP 500 for CertificateRequest and CSR imports. | [Migration 36332106663](https://github.com/centerionware/not-k8s/actions/runs/36332106663); logs `/tmp/nodemigrate-36332106663/nodemigrate-k3s-36332106663/nodemigrate-k3s.log` and `/tmp/nodemigrate-36332106663/nodemigrate-kubernetes-36332106663/nodemigrate-kubernetes.log` |
| `7ba45556862d3ecd90cffe22cde4ac4d71a9f71b` | Branch-runtime K3s+Cilium and upstream+Cilium migration | Both lanes passed source StatefulSet PVC/PV/data checks and reached nodestore verification, then failed because the hostpath `CSINode` lacked its Node owner reference. No nodestore storage checkpoint or return leg passed. | [Migration 36308845021](https://github.com/centerionware/not-k8s/actions/runs/36308845021); logs `/tmp/nodemigrate-36308845021/artifacts/` |
| `f8ff6a93b719453df806ee366d5d6403829d6bb8` | APIService freshness quick-check and migration run | Focused `nodeapiserver` quick-check passed. Dedicated migration run failed both lanes after branch builds; five-node Docker preflight passed. K3s still omitted metrics resources even after APIService `Available=True`, disproving stale reflector state as the failure cause; both lanes also reproduced CSI NodeStageVolume volume-catalog misses. Neither lane completed a return migration. | [Quick-check 36301964871](https://github.com/centerionware/not-k8s/actions/runs/36301964871); [migration 36301989587](https://github.com/centerionware/not-k8s/actions/runs/36301989587); logs `/tmp/nodemigrate-36301989587/`. |
