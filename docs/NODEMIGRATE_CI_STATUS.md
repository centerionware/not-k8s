# nodemigrate CI and integration status

Last updated: 2026-09-25

This is the living CI record for the scope in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). The user-specific testing policy
is recorded there and overrides conflicting general `AGENTS.md` gates for
this objective.

The [bug and fix tracker](NODEMIGRATE_BUGS.md) lists confirmed defects by
owning component, branch fix, and focused test evidence. The target is the
coordinated `v0.8.1` runtime and standalone utility; `v0.8.0` remains the
release-backed regression baseline, not a claim that its runtime contains the
new fixes.

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

The latest branch-runtime run used SHA `e229ae742ab64327512086decdb9a08d58dbfe81`
in [run 36089689047](https://github.com/centerionware/not-k8s/actions/runs/36089689047).
The nodemigrate and combined runtime release builds passed in both lanes, and
the five-node Docker isolation preflight passed. K3s source setup, Cilium,
workloads, and forward migration completed. Both lanes failed after cutover
because Cilium Envoy could not create `/var/run/cilium/envoy/sockets` under its
read-only root. The attempted parent-first sort passed nodelet quick-check
[36089689032](https://github.com/centerionware/not-k8s/actions/runs/36089689032),
but this rerun reproduced the same error. Captured Pod YAML shows the parent
mount is in the separate Cilium agent Pod, not Envoy's Pod; inspect Envoy's own
CRI/OCI request before another fix. CNI/CoreDNS did not become ready, so
hostpath CSI, post-cutover workload checks, reverse migration, and semantic
comparison did not run. Full logs are saved at
`/tmp/nodemigrate-36089689047/artifacts/`. No local Cargo test/build was run.

The previous branch-runtime run used SHA `4a4c75e8fa9dab61e62672a14aa956ef7e16b74b`
in [run 36088034808](https://github.com/centerionware/not-k8s/actions/runs/36088034808).
It confirmed the bounded node-IP wait works, then found the same Envoy failure.
Logs are saved at `/tmp/nodemigrate-36088034808/artifacts/`.

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

For every new result, record the commit SHA, workflow run URL, lane, resolved
Kubernetes/K3s/Cilium/add-on versions, and pass/fail state at each checkpoint.
Keep failures and skipped checks visible rather than replacing them with a
later green run.
