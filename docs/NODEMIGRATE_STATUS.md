# nodemigrate status dashboard

Last updated: 2026-10-01

This dashboard tracks the full nodemigrate goal in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). Detailed status is kept in the
separate living documents below.

## Current state

The latest completed migration run, [36805374812](https://github.com/centerionware/not-k8s/actions/runs/36805374812), tested branch-built components with Cilium KPR and the required five-node topology of three control planes plus two workers. K3s completed its round trip. The five-node lane passed setup, forward migration, workload checks, and all three control-plane returns, then exposed a `nodemigrate` validation bug when `worker-1` was incorrectly rejected for `skip-api-export`. The worker validation fix and regression passed focused quick-check [36809748353](https://github.com/centerionware/not-k8s/actions/runs/36809748353) at SHA `eca452a1`. Upstream completed import and Cilium agent/socket recovery but its replacement Envoy Pod crashed; termination output was not captured, so the cause remains unknown. Two no-migration probes [36810231370](https://github.com/centerionware/not-k8s/actions/runs/36810231370) and [36812017038](https://github.com/centerionware/not-k8s/actions/runs/36812017038) both kept Envoy Ready after Cilium cleanup, but their broad fixture recovery checks failed on CoreDNS. The scoped probe [36814377031](https://github.com/centerionware/not-k8s/actions/runs/36814377031) passed graceful Envoy replacement readiness; [36815693628](https://github.com/centerionware/not-k8s/actions/runs/36815693628) exposed an unsupported `kubectl` flag before deletion. The corrected immediate UID-preconditioned API delete and Envoy replacement passed in [36817021594](https://github.com/centerionware/not-k8s/actions/runs/36817021594) at SHA `3d77f575`. The migration-specific Envoy failure remains unverified; the full migration matrix is next. No regular build or general e2e gate ran.

The prior Docker failure in run [36578066781](https://github.com/centerionware/not-k8s/actions/runs/36578066781) exposed a two-second CRI removal timeout while stopping Cilium. The branch now gives sandbox stop/removal 60 seconds and removes the local Cilium agent last. Nodemigrate-only quick-check [36582679193](https://github.com/centerionware/not-k8s/actions/runs/36582679193) passed all 115 tests. The latest migration run confirms the timeout/order fix did not regress either single-node round trip.

Control-plane cleanup remains conflict driven: export API state and snapshot affected local data/configuration first; preserve source quorum and rollback material until the destination has quorum and its API/data are verified; then retire old services, manifests, sockets, or member state that conflict with the verified target. Keep etcd/datastore data and PKI while rollback or a return migration is needed. A real multi-control-plane host may require retiring conflicting old control-plane parts; complete five-node retirement and return remain unverified.

No regular build gate or full e2e workflow ran. The nodemigrate-specific migration workflow is the runtime test, and its branch combined build compiles the packaged runtime components. Standalone release policy is recorded separately; no publication was authorized or performed.

| Area | State | Detail |
| --- | --- | --- |
| Full bidirectional migration implementation | Latest run 36805374812: K3s passed; five-node reached reverse worker return but hit role validation; upstream Envoy replacement crashed after import. The worker fix passed focused CI; Envoy cause is unknown. | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| Workload and API-kind parity | The fixture includes the required workload, storage, Helm, routing, RBAC/admission, CRD, and discovered-resource checks. Five-node API parity now exposed control-plane join overwrites and a known CSI fixture mount delta; focused checks and a clean strict comparison are pending. | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| Bugs found and component fixes | Run 366504 exposed an internal Pod binding storage conflict and a kubelet-to-Nodelet CRI label handoff failure. Both fixes passed focused `nodeapiserver,nodelet` quick-check; migration runtime evidence is pending. | [Bug and fix tracker](NODEMIGRATE_BUGS.md) |
| Existing nodestore member replacement and new control-plane joins | Node replacement ordering and Raft learner catch-up/promotion logic are implemented. The existing-cluster join/replacement scenario remains unverified by a completed migration run. | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| Worker-node migration | K3s agent, upstream kubelet, and not-k8s nodelet roles are inventoried; paths avoid cluster-wide re-import and preserve node-local PV data. Multi-node worker replacement behavior remains unverified. | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| K3s external-CNI uninstall preservation | Configured CNI paths are detected and snapshotted/restored around explicit uninstall. Cilium migration without uninstall passed the single-node round trip; explicit K3s+Cilium uninstall remains unverified. | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| K3s/Cilium and upstream Kubernetes/Cilium test lanes | K3s passed in 36805374812. Upstream Envoy replacement crashed after import; root cause is not captured. | [CI status](NODEMIGRATE_CI_STATUS.md) |
| Nodemigrate merge gates | K3s single-node round trip passed in 36805374812. The three-control-plane/two-worker round trip is incomplete due to worker role validation; upstream Envoy crash diagnosis is also pending before another matrix run. | [CI status](NODEMIGRATE_CI_STATUS.md) |
| Docker five-node isolation and migration | The 3-CP/2-worker topology passed setup, forward migration, and control-plane return; reverse worker return exposed a role-validation bug. | [CI status](NODEMIGRATE_CI_STATUS.md) |
| Standalone artifact and shared-version behavior | Release design present; nodemigrate publication not recorded | [Release status](NODEMIGRATE_RELEASE_STATUS.md) |

## Current verification

- Follow-up branch build and utility test
  [36389615059](https://github.com/centerionware/not-k8s/actions/runs/36389615059)
  and [36389588658](https://github.com/centerionware/not-k8s/actions/runs/36389588658)
  at SHA `f8cc4777` both failed before runtime tests because of the same
  `anyhow::Error` double-wrap compile error. It is corrected locally and awaits
  CI validation. This attempt gives no runtime migration evidence.

- Latest branch-runtime run
  [36385002094](https://github.com/centerionware/not-k8s/actions/runs/36385002094)
  at SHA `9c7897f0d917e51b9e3d8e81ee6a843e3332c8f2` built `nodemigrate` and
  branch `notk8s --features cri` in all migration lanes. This compiles all
  runtime component crates packaged by the combined binary, including branch
  changes; a separate selected-component build is unnecessary. The targeted
  quick-check passed at the same SHA in
  [36384989298](https://github.com/centerionware/not-k8s/actions/runs/36384989298).
  K3s then timed out on full API discovery after direct API readiness passed.
  Upstream passed return-stage behavior and resource checks but failed strict
  parity. See [migration status](NODEMIGRATE_MIGRATION_STATUS.md) for exact
  differences. General build and full e2e workflows were not run.

- Branch-runtime run
  [36366505575](https://github.com/centerionware/not-k8s/actions/runs/36366505575)
  at SHA `50b2805f` passed nodemigrate/branch-runtime builds and Docker
  kubeadm preflight. K3s passed direct Namespace readiness but all 20 bounded
  destination API discovery probes timed out over five minutes; post-failure
  diagnostics show Cilium agent and Envoy unhealthy. Upstream recovered API
  readiness after one connection-refused probe, then failed applying
  `CertificateRequest migration-test-1` because the cert-manager webhook
  ClusterIP timed out. Both lanes rolled back; the downstream workload check
  still found an exited CRI container. The focused nodemigrate quick-check
  passed at code SHA `801b3275` in
  [36366260390](https://github.com/centerionware/not-k8s/actions/runs/36366260390).
  Full e2e and general build workflows were not run. Artifacts are in
  `/tmp/nodemigrate-36366505575/`.

- Branch-runtime run
  [36363999391](https://github.com/centerionware/not-k8s/actions/runs/36363999391)
  at SHA `27b4152b` passed nodemigrate and branch-runtime builds, and the
  Docker kubeadm preflight passed. K3s passed retained API readiness using the
  direct Namespace probe, then destination API discovery returned HTTP 503
  before Namespace import and rolled back. Upstream's API probe recovered
  after one connection-refused attempt and accepted all 55 CRD apply requests;
  importing `CertificateRequest migration-test-1` failed because the
  cert-manager webhook ClusterIP timed out. Both lanes rolled back. A bounded
  retry for initial destination discovery passed focused nodemigrate
  quick-check [36366260390](https://github.com/centerionware/not-k8s/actions/runs/36366260390)
  at SHA `801b3275`. Live validation is pending. Artifacts are in
  `/tmp/nodemigrate-36363999391/`.

- Branch-runtime run
  [36361932369](https://github.com/centerionware/not-k8s/actions/runs/36361932369)
  at SHA `890e8d65` passed nodemigrate and branch-runtime builds. K3s source,
  forward migration, and nodestore fixture checks passed. During return, all
  20 retained-API probes hit their ten-second deadline while `ready()` ran API
  discovery; the five-minute retry window then rolled back to nodestore. The
  post-rollback `emptyDir` exec failed because the selected container was in
  `CONTAINER_EXITED`. The upstream lane failed importing a CertificateRequest
  with API HTTP 500. Five-node Docker preflight failed in its kubeadm probe.
  Focused nodelet quick-check [36361932310](https://github.com/centerionware/not-k8s/actions/runs/36361932310)
  passed at the same SHA. The readiness probe now lists a core Namespace
  directly; focused nodemigrate quick-check
  [36363774087](https://github.com/centerionware/not-k8s/actions/runs/36363774087)
  passed at SHA `2884eeaa`. Full e2e and general
  build workflows were not run. Logs are in
  `/tmp/nodemigrate-36361932369/`.

- Branch-runtime run
  [36359977515](https://github.com/centerionware/not-k8s/actions/runs/36359977515)
  at SHA `f538a8d3` passed utility, combined-runtime, and five-node container
  image builds. Both K3s and upstream lanes passed source and first nodestore
  fixture checkpoints, including the `emptyDir` marker exec. K3s return timed
  out 20 retained-API probes; upstream return hit API connection refusal and
  then cert-manager webhook timeouts during import. Both restored nodestore,
  but post-rollback `emptyDir` exec failed against stale/missing CRI workload
  state. Five-node kubeadm/Cilium preflight reached five Ready nodes and
  recovered after cp-1 loss, then failed because the hostpath setup file was
  requested from `/tmp` after being copied to `/var/tmp`. Nodelet now shares
  running/newest container selection with reconciliation, and the fixture now
  uses the configured helper path. Focused nodelet validation and live rerun
  are pending. Artifacts are saved under `/tmp/nodemigrate-36359977515/`.

- Branch-runtime run
  [36357521280](https://github.com/centerionware/not-k8s/actions/runs/36357521280)
  at SHA `22848696` passed the `nodemigrate` quick-check and both focused
  migration utility/runtime builds, but the Docker preflight and both migration
  lanes ended in failure. Five-node Cilium setup and recovery passed before
  `docker cp` collided on the hostpath helper's `/tmp` path. Both single-node
  lanes reached their `stage=nodestore` fixture, then `kubectl exec` selected
  an unavailable `emptyDir` test container. Code inspection points to CRI
  lookups choosing stale sandboxes or container attempts. Nodelet now selects
  by current Pod UID when available and prefers ready sandboxes and running
  container attempts. Quick-check [36359505569](https://github.com/centerionware/not-k8s/actions/runs/36359505569)
  found selector visibility and borrow/move compile errors; both were corrected
  in `8f7df53e`, and `nodelet` quick-check passed in
  [36359755288](https://github.com/centerionware/not-k8s/actions/runs/36359755288).
  The migration rerun is pending. Logs
  are saved under `/tmp/nodemigrate-36357521280/`. No general build or full
  e2e gate was run.

- Branch-runtime run
  [36355485146](https://github.com/centerionware/not-k8s/actions/runs/36355485146)
  at SHA `26f81b53` used `runtime_source=branch`, `cilium_kpr=true`, and
  `five_node_migration=true`. Both single-node lanes passed forward migration
  and their nodestore checkpoints. K3s return API readiness failed with
  Service traffic unreachable; upstream imported the recreated token Secret
  but its returned Node did not become Ready. The five-node Kubeadm/Cilium
  preflight passed, then the migration fixture failed because its hostpath
  setup script was absent after `cp-1` restart. No bidirectional acceptance
  gate passed. The focused `nodemigrate` quick-check at SHA `26f81b53` passed in
  [36355485143](https://github.com/centerionware/not-k8s/actions/runs/36355485143).

- Release-backed run
  [36351022258](https://github.com/centerionware/not-k8s/actions/runs/36351022258)
  at SHA `55978b86` against released `v0.8.0` and Cilium KPR enabled is
  terminal failure. The K3s lane exhausted 59 five-second retries while two
  restored Gateway API objects remained unavailable because the destination
  rejected three Gateway API CRDs: CEL validation reported invalid map/object
  comprehension types and estimated-cost overflow. The upstream lane also
  failed during import. This run did not exercise the retained-API return wait
  or complete either round trip; it is release-baseline evidence, not a test of
  branch nodeapiserver fixes.

- At code SHA `2d98e05edc709b7aeec145d652b29aa1fe0137a9`, focused
  `nodemigrate` and `nodebootstrap` tests passed in
  [36350477684](https://github.com/centerionware/not-k8s/actions/runs/36350477684).
  The PR's targeted `nodemigrate` test passed at SHA `c9007b9c` in
  [36350708110](https://github.com/centerionware/not-k8s/actions/runs/36350708110).
  The fix bounds each API readiness probe to ten seconds and promotes a newly
  joined control-plane member after Node readiness. Run
  [36343008296](https://github.com/centerionware/not-k8s/actions/runs/36343008296)
  is terminal cancelled after its K3s return leg stalled at retained API
  readiness; the artifact contains no underlying API error, so runtime behavior
  remains unverified. No general build or e2e gate was run.

- At code SHA `f5bcadbbff5029c6128544a771ca2bd1ba1114ca`, focused nodemigrate
  tests passed in
  [36349107771](https://github.com/centerionware/not-k8s/actions/runs/36349107771).
  Migration workflow validation passed in
  [36349107726](https://github.com/centerionware/not-k8s/actions/runs/36349107726);
  migration and Docker runtime jobs were skipped for the pull-request event.
  The multi-node runtime gate remains unverified.

- At code SHA `59b38a9604e20cae478d35ce737ef4c189960bf2`, the export-copy
  regression passed in focused nodemigrate crate run
  [36348554916](https://github.com/centerionware/not-k8s/actions/runs/36348554916).
  Migration workflow validation passed in
  [36348554980](https://github.com/centerionware/not-k8s/actions/runs/36348554980);
  its Docker and migration jobs were skipped for the pull-request event.
  The copy isolation has no multi-node runtime evidence yet.

- At PR code SHA `7bad3b9145dd5a6b0612d8eeeca2ef0fd06bbdcf`, the proxy ownership
  fix and KPR `sudo` environment forwarding passed targeted nodemigrate crate
  tests in [36347140648](https://github.com/centerionware/not-k8s/actions/runs/36347140648)
  and migration workflow validation in
  [36347140725](https://github.com/centerionware/not-k8s/actions/runs/36347140725).
  Run [36335580680](https://github.com/centerionware/not-k8s/actions/runs/36335580680)
  confirms the pre-fix dual-proxy runtime bug. The current ownership fix has
  not yet run against a live migration target.

- At code SHA `3cb608ab7276a1cfcd1dd012f5f17c3f2d584711`, targeted
  nodemigrate crate checks passed in
  [36347577818](https://github.com/centerionware/not-k8s/actions/runs/36347577818)
  and migration workflow validation passed in
  [36347577796](https://github.com/centerionware/not-k8s/actions/runs/36347577796).
  These are code/static checks, not runtime migration evidence. Run
  [36343008296](https://github.com/centerionware/not-k8s/actions/runs/36343008296)
  is now cancelled: the K3s lane passed forward migration then stalled for over
  an hour waiting for retained API readiness during return; the upstream lane
  failed earlier. The K3s artifact ends at the readiness wait, without an API
  probe error. Later focused CI validated the bounded probe at SHA `2d98e05e`
  in run `36350477684`; a live post-fix return remains pending.

- At branch SHA `bb32d6c86dfe614b87fa66edf26d313b3b1fa5ad`, the migration
  watcher now records kube-proxy DaemonSet and Pod readiness during target
  snapshots. Local shell syntax, diagnostic JSON regression, diff whitespace,
  and commit-subject checks passed, as did migration workflow validation
  [36339547744](https://github.com/centerionware/not-k8s/actions/runs/36339547744)
  and targeted `nodemigrate` crate checks
  [36339547796](https://github.com/centerionware/not-k8s/actions/runs/36339547796).
  No runtime result is claimed for this instrumentation. Run
  [36335580680](https://github.com/centerionware/not-k8s/actions/runs/36335580680)
  is terminal failure (upstream failed, K3s was cancelled at timeout); its
  artifacts were retrieved once for review. The newer KPR-enabled run
  [36343008296](https://github.com/centerionware/not-k8s/actions/runs/36343008296)
  remains active in K3s, so another long migration run has not been started.

- Branch-runtime migration [36260417450](https://github.com/centerionware/not-k8s/actions/runs/36260417450)
  at SHA `8c79470de60f288fc113db7b7b8c45da048b6ad7` passed the upstream source
  and nodestore target checkpoints, then failed reverse import after the
  five-minute retry window. Certificate, CertificateRequest, and ClusterIssuer
  writes timed out at the webhook ClusterIP; a PersistentVolume write returned
  404. The upstream lane did not reach returned-source checks or parity; K3s is
  still in progress. Commits `058fafa8` and `434c9acb` add return-leg webhook
  probes and request-path diagnostics. Commit `3f7c8873` adds reverse-cutover
  rollback; its focused nodemigrate tests and migration workflow validation
  passed in [36264669564](https://github.com/centerionware/not-k8s/actions/runs/36264669564)
  and [36264669587](https://github.com/centerionware/not-k8s/actions/runs/36264669587).
  That older runtime run predates the rollback fix, so runtime recovery remains
  unverified. No regular build or full e2e gate was run.

- Migration [36253413938](https://github.com/centerionware/not-k8s/actions/runs/36253413938)
  at SHA `19e1cae66e57ed9abf02625d8ec9ba877f30e651` passed Docker isolation
  and both utility/runtime builds, then failed target Ingress probes in both
  lanes. Retrieved raw target JSON proves the host rule survived but its
  embedded HTTP paths/backend were lost; `IngressClass.spec.controller` was
  correct. This confirms a `nodeapiserver` protobuf-codec bug. The preceding
  run [36251890971](https://github.com/centerionware/not-k8s/actions/runs/36251890971)
  showed query-free SPDY port-forwarding works through Gateway API (HTTP 200).
  A codec fix/regression and moving the exact spec assertion to every stage
  passed its focused quick-check at `4984eb2e` in
  [36255128063](https://github.com/centerionware/not-k8s/actions/runs/36255128063).
  Dedicated migration rerun [36255128061](https://github.com/centerionware/not-k8s/actions/runs/36255128061)
  has passed Docker preflight and is compiling/running both lanes. No target
  semantic checkpoint, return migration, or round trip has passed yet.

- Branch-runtime migration [36229667964](https://github.com/centerionware/not-k8s/actions/runs/36229667964)
  used head SHA `294a6c64`. Docker five-node preflight and both utility and
  combined-runtime builds passed. K3s accepted all 59 CRDs and reached target
  API readiness; the Node reported Ready, and Cilium agent, Envoy, and operator
  reached `1/1`. The mounted service-account CA fingerprint read from the live
  Cilium agent exactly matched the destination API CA fingerprint. CoreDNS
  Pods remained `Running` but `0/1 Ready`; nodelet logs show ordinary Pod
  reconciliation remained paused behind its CoreDNS readiness gate, and
  hostpath CSI setup failed. Upstream accepted all 55 CRDs, then failed
  importing `CertificateRequest/migration-test-1` because the cert-manager
  webhook Service had no endpoints. Both source services recovered and
  protected exports were retained. Neither lane reached workload/storage
  parity, return migration, or a round trip. This branch run changes the
  current diagnosis: the measured K3s Cilium CA was correct; the reason
  CoreDNS stayed unready remains unproven. CoreDNS logs say its Kubernetes
  plugin was waiting for API synchronization and the ready plugin was not
  ready, without exposing the failed request's underlying error. Logs are under
  `/tmp/nodemigrate-36229667964/`.

- Release-backed migration [36228920539](https://github.com/centerionware/not-k8s/actions/runs/36228920539)
  at SHA `f86fad5622d48582c24531df4b88368096d632d9` fetched `v0.8.0`; utility
  builds and the Docker five-node preflight passed. K3s source fixture passed
  and nodemigrate captured 582 objects/59 CRDs. Three Gateway API CRDs were
  rejected by v0.8.0's unsupported CEL `matches`, so the target could not
  import the matching Gateway object. During the target window the API had no
  schedulable nodes; Cilium and cert-manager did not reach running status and
  the cert-manager webhook had no endpoints. Mounted-CA probing was blocked:
  nodelet's port 10250 refused `kubectl exec`; the later Ready Node was observed
  after source rollback. Upstream again failed CertificateRequest and CSR
  imports with HTTP 500 and hit the same Gateway API CEL failures. Both sources
  recovered and protected exports were retained. Neither lane reached a target
  workload/storage checkpoint, reverse migration, or parity. The logs therefore
  do not prove a mounted-CA mismatch; the no-node/readiness failure is the
  stronger live target evidence.

- Release-backed migration [36228127861](https://github.com/centerionware/not-k8s/actions/runs/36228127861)
  at SHA `d92abf28397e143c266c115cf2deb7d694f23374` fetched regular release
  `v0.8.0`; Docker isolation and both nodemigrate builds passed. Source-stage
  CA checks passed in both lanes. K3s import continued to show Cilium/Traefik
  TLS `unknown authority` errors despite the importer seeding the target CA
  ConfigMaps. Upstream failed importing CertificateRequest and CSR objects with
  HTTP 500; its current Gateway API CRDs were also rejected because v0.8.0's
  CEL runtime does not implement `matches`. Both sources recovered and kept
  protected exports. Neither lane reached target workload/storage checks,
  reverse migration, or parity. The run disproves the sufficiency of matching
  ConfigMap data as evidence that running Pods trust the active API CA; the
  harness now fingerprints mounted service-account CA files for Cilium and
  cert-manager during the next run.

- Dedicated migration run [36226000144](https://github.com/centerionware/not-k8s/actions/runs/36226000144)
  passed both builds, Docker isolation, and source-stage CA checks. Destination
  CA ConfigMaps matched the target kubeconfig, but Cilium/Traefik still
  rejected the API certificate and both CertificateRequest imports failed
  because the cert-manager webhook was unreachable. Rollback restored both
  sources and retained exports. The importer now ensures destination CA
  bundles before applying workloads; focused and runtime checks remain pending.
  No target workload/storage, reverse-migration, or parity checkpoint passed.

- Dedicated migration run [36223445443](https://github.com/centerionware/not-k8s/actions/runs/36223445443)
  passed the five-node Docker isolation preflight and both utility/combined
  runtime builds. K3s reached target API readiness but failed at hostPath CSI;
  upstream failed importing `CertificateRequest/migration-test-1` and restored
  its source. Target Cilium/Traefik logs show API Service TLS trust failures.
  Nodeproxy started successfully, so its post-rollback inactive status was not
  causal evidence. The branch now regenerates destination `kube-root-ca.crt`
  bundles and checks them against the active API CA; targeted CI/runtime
  verification is pending. No target workload, reverse migration, or parity
  checkpoint passed. See [CI status](NODEMIGRATE_CI_STATUS.md).

- At SHA `f9e4b31139a30fb7a453161e4dc2295983fcef8f`, the nodemigrate crate
  tests and quick-check for `nodeapiserver,nodecontroller,nodebootstrap` passed
  in runs [36071161354](https://github.com/centerionware/not-k8s/actions/runs/36071161354)
  and [36071174298](https://github.com/centerionware/not-k8s/actions/runs/36071174298).
  The release-backed retry in [run 36071174265](https://github.com/centerionware/not-k8s/actions/runs/36071174265)
  reproduced v0.8.0's CSR codec failure, and confirmed source service/API
  recovery plus protected-export retention. K3s forward migration passed, but
  CSI workload checks still hit the v0.8.0 controller-manager 403. The Docker
  container isolation preflight now passes, but Kubernetes/Cilium five-node
  migration and all full round trips remain unverified.

- At SHA `dd31443360d6d3412783ebe27d7b44661f496eef`, the Docker-only
  preflight passed all five containers' systemd, CRI, BPF, namespace,
  persistent-volume, peer-connectivity, and stop/restart isolation checks in
  [run 36077448685](https://github.com/centerionware/not-k8s/actions/runs/36077448685).
  The K3s and upstream lanes were intentionally skipped for this focused
  harness check. Their latest `v0.8.0` outcomes remain in the CI record.

- Compatible API-version import selection was added at SHA
  `954f1be3d12fb7f0334db8dff6efae28ca0e4250`. Focused nodemigrate tests passed
  in [run 36068373192](https://github.com/centerionware/not-k8s/actions/runs/36068373192).
  The release-backed run [36068417485](https://github.com/centerionware/not-k8s/actions/runs/36068417485)
  failed in both single-node lanes. K3s forward migration passed, then v0.8.0
  denied CSI pod creation by `system:kube-controller-manager` (403). Upstream
  migration used the compatible ClusterTrustBundle v1beta1 API, then the
  v0.8.0 server returned 500 on CSR `ExtraValue`; the current PR's
  nodeapiserver fix is not in that release. The Docker image built, but systemd
  again exited 255 before five-node isolation checks.

- At `336ea350502288d55bb6a57763494fa0aa89a475`, focused quick-check for
  `nodeapiserver,nodemigrate` passed ([run
  36065050713](https://github.com/centerionware/not-k8s/actions/runs/36065050713)).
  Release-backed run [36065059567](https://github.com/centerionware/not-k8s/actions/runs/36065059567)
  built the utility and verified `v0.8.0`. K3s forward migration completed
  with all 48 CRDs accepted; the fixture then failed while redeploying CSI
  because it reapplied migrated VolumeSnapshot CRDs and v0.8.0 returned 404
  for VolumeSnapshotClass. Upstream import reached two remaining object
  failures: CertificateSigningRequest ExtraValue encoding and unavailable
  ClusterTrustBundle/v1. The Docker probe again exited 255 before systemd
  readiness. Server-side CRD-update and ExtraValue fixes are in the PR code,
  but cannot change the released runtime under test. No round trip passed.
- At `e6963be70467318f19b8c9fb8ef7197c0c9980b2`, run
  [36066311951](https://github.com/centerionware/not-k8s/actions/runs/36066311951)
  confirmed the fixture no longer reapplies migrated snapshot CRDs and the
  restored VolumeSnapshotClass is usable. K3s then stopped because the
  v0.8.0 target returned 403 for `system:kube-controller-manager` creating
  CSI pods. Upstream still failed only on CSR ExtraValue and
  ClusterTrustBundle/v1. Docker systemd readiness also failed again.
  Reverse migration and round-trip comparisons remain unverified.

- At `093f2e440b16c4a2a585b424b816b97ab49480a9`, focused nodemigrate crate
  tests, packaging, integration shell validation, and commit convention passed
  (runs `36058334362`, `36058334455`, and `36058331222`). Release-backed run
  [36058570338](https://github.com/centerionware/not-k8s/actions/runs/36058570338)
  built the utility and verified the latest regular `v0.8.0` runtime. Both
  source setups and target bootstraps passed, but API import failed after a
  60-second CRD discovery wait: 37 K3s objects and 26 upstream objects remained
  pending. The run captured 506/48 objects/CRDs from K3s and 488/44 upstream.
  It does not establish whether each CRD write persisted or why discovery did
  not expose the APIs. The Docker preflight again exited before systemd
  readiness. No local Cargo build or test was run.

- Targeted quick-check passed at `daac9ad05285e6ff749d4c51a18f9b71c8fb7dee`
  before bidirectional migration changes.
- The crate check at `55704b1c202c248ab633aac8c74877c46499e59f` failed to
  compile; those compiler errors were corrected. At
  `4e932917080e21412c79625fb2b79d08f5e62c0f`, compilation succeeded but the
  upstream control-plane detection test exposed a rooted-path bug. The fix
  passed nodemigrate crate tests at `4ef3585eaea6beae51ffd1116134435b0edc305e`.
- Source CNI detection and the joined replacement node-agent path passed the
  focused nodemigrate tests at `63cb20cbe75ebdfafee861c41137b334fb6ff95b`.
- The Cilium health assertions passed shell syntax validation at
  `21d532991835ff587a918e3bf665ed4f601e6fdf`. The manual release-backed
  attempt at `fc161b8c4262cdeb251abf6bbf2090e180d56c55` failed before migration:
  the K3s lane could not start hostPath CSI setup pods because the bridge CNI
  plugin was missing; the kubeadm lane's Cilium config init container and
  operator crashed. See [run 36034461348](https://github.com/centerionware/not-k8s/actions/runs/36034461348).
- Control-plane join import control passed the focused nodemigrate crate
  checks at `0fe454dad5b3fb194b72a48bc657ed0512a7f29a` (run
  [35962922792](https://github.com/centerionware/not-k8s/actions/runs/35962922792));
  PR shell validation ([35962922860](https://github.com/centerionware/not-k8s/actions/runs/35962922860))
  and commit convention ([35962919894](https://github.com/centerionware/not-k8s/actions/runs/35962919894))
  passed. The multi-node runtime behavior remains unverified.
- The reverse staged-control-plane and node-affinity PV changes passed focused
  nodemigrate checks at `7e16754501e87d5983dcb0335b6d972529dec5b9` ([run
  35965492161](https://github.com/centerionware/not-k8s/actions/runs/35965492161));
  shell validation and commit convention passed too. Runtime migration remains
  unverified. See the [CI record](NODEMIGRATE_CI_STATUS.md).
- Control-plane migration could accept an imported stale Ready Node as proof
  the replacement joined. The current fix requires explicit
  `NODEMIGRATE_REPLACE_NODE=true` and deletes that object before waiting for
  fresh registration in both directions when the target API is ready; reverse
  control-plane returns require that confirmation even when the retained API
  is stopped. The replacement restores node labels, taints, and unschedulable
  state after registration. Focused nodemigrate checks passed at
  `2a9b26c3061e86c29d9f601b3bc7a461a8e718dc` ([run
  35968225182](https://github.com/centerionware/not-k8s/actions/runs/35968225182));
  staged quorum orchestration and runtime behavior remain unverified.
- At `8518a99537cc4b98f2cbf8184f49c9927a8d78cf`, nodemigrate checks, shell
  syntax validation, and commit convention passed (runs `35956413580`,
  `35956413566`, and `35956412012`). The replacement-membership changes now
  committed at `c468627045cae98360bed8a93397407ff934ed86` passed nodemigrate
  checks (`35957207376`), quick-check for `nodebootstrap,nodestore`
  (`35957212012`), PR shell validation (`35957207356`), and commit convention
  (`35957206090`). The real replacement-member runtime case remains unverified.
- The user directed that general e2e and build gates not run. They authorized
  the dedicated migration runtime workflow; its first release-backed attempt
  failed during source cluster setup before the migration utility ran. A
  second attempt at `c6517316` exposed CNI-path, CSI plugin-directory, and
  kubeadm API endpoint fixture issues. Run `36039647520` stopped in CNI plugin
  path setup and Docker mount syntax. Run `36040401537` exposed a topology
  registration gap and a package conflict. Run `36042056929` passed source
  Cilium and CSI readiness in both lanes, then failed on the archived setup's
  nodelet DRA registration requirement. Run `36043310369` passed all K3s
  source-stage checks and both nodemigrate commands ran; selecting rustls ring
  fixed the first panic. Tower then panicked because client construction did
  not enter the Tokio runtime. The utility now enters that runtime while
  constructing the Kubernetes client and has a focused regression test. Docker
  still exits systemd with code 255 without logs; the entrypoint now prints its
  resolved binary path and enables debug output. Runtime retest is pending.
  See [run 36045632585](https://github.com/centerionware/not-k8s/actions/runs/36045632585).
- The current worktree adds protected export UID metadata, Node replacement
  state preservation and UID preconditions, and full migratable-object
  fingerprints in the integration fixture. Focused snapshot-filter checks,
  shell syntax, Rust formatting, and whitespace checks pass locally. Targeted
  nodemigrate checks ran at `caed49582952a0743d87a06bbb52fceb737c059e`
  ([run 35972976396](https://github.com/centerionware/not-k8s/actions/runs/35972976396));
  packaging and detection passed, but crate tests failed. The captured
  diagnostic showed test temporary directories were group-readable, causing
  the protected-export loader to reject one fixture; a second path-traversal
  fixture had been passing on that same early permission check. Both fixtures
  now use the production mode `0700`. Focused nodemigrate checks passed at
  `e943bb756ef8568c4e4bd89ee2bb6d16c165107f` ([run
  35976429487](https://github.com/centerionware/not-k8s/actions/runs/35976429487));
  release policy ([35976429468](https://github.com/centerionware/not-k8s/actions/runs/35976429468))
  and shell/snapshot validation ([35976429428](https://github.com/centerionware/not-k8s/actions/runs/35976429428))
  passed too. No Cargo build or test was run locally.
- External CNI path forwarding and K3s data-dir backup/restore passed focused
  nodemigrate checks at `c2df44bfd41f7e34f5eb92ce52a2196eb36849a3`
  ([run 36026420825](https://github.com/centerionware/not-k8s/actions/runs/36026420825)).
  The nodebootstrap containerd CNI config tests passed in targeted quick-check
  at `bb82cea88f48d73ca0bbb5ee00c0fd35a00fac20`
  ([run 36025823559](https://github.com/centerionware/not-k8s/actions/runs/36025823559));
  nodebootstrap was unchanged after that SHA. No real K3s uninstall or Cilium
  runtime migration was run.
- The five-node Docker preflight image/script and manual workflow job were
  added at `ad509e5b`. Targeted nodemigrate checks passed ([run
  36031941620](https://github.com/centerionware/not-k8s/actions/runs/36031941620));
  PR validation passed shell syntax and snapshot checks ([run
  36031941811](https://github.com/centerionware/not-k8s/actions/runs/36031941811)).
  The manual Docker preflight was skipped, so isolation capability remains
  unverified.
- Protected export format v2 records every source Node's scheduling metadata
  for later offline control-plane and worker joins after source API quorum is
  lost. Export serialization and load passed at `ac5a05b4` ([run
  36028447623](https://github.com/centerionware/not-k8s/actions/runs/36028447623));
  worker join validation and node-affined hostPath backup/restore passed at
  `a360ea6c` ([run
  36029431383](https://github.com/centerionware/not-k8s/actions/runs/36029431383)).
  Runtime quorum-loss and volume recovery remain unverified.

## Next actions

1. Resolve the K3s returned-Node disappearance before retrying migration.
   Saved runs show the Node Ready and then absent, but do not establish the
   deleting actor or cause. Node/Lease audit capture and watcher identity
   diagnostics are in the fixture and have not yet been exercised in a
   migration. Preserve readiness and storage assertions.
2. Keep the confirmed component defects fixed together before the next
   migration batch: CronJob no-op status writes, omitted CSINode drivers,
   exact JSON float round trips, and PVC-before-PV claim UID remapping. Focused
   tests passed; runtime data handoff and strict parity remain unverified.
3. After the K3s cause and batch fixes are addressed, run the dedicated
   branch-runtime K3s+Cilium and upstream Kubernetes+Cilium round trips. Verify
   source, not-k8s, and returned-source workloads, Helm state, network paths,
   PV/PVC bindings, and payload data at each checkpoint.
4. Complete the isolated upstream three-control-plane/two-worker migration,
   including existing-cluster join/replacement and membership recovery.
   Docker's five-node preflight proves isolation only, not migration behavior.
5. Keep both required runtime merge gates and release readiness open until
   their full evidence passes. The regular build and full e2e gates remain
   excluded for this objective; a nodemigrate-only release must not advance
   shared `VERSION`.
