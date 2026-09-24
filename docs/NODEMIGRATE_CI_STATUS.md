# nodemigrate CI and integration status

Last updated: 2026-09-24

This is the living CI record for the scope in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). The user-specific testing policy
is recorded there and overrides conflicting general `AGENTS.md` gates for
this objective.

## Workflow

- `.github/workflows/nodemigrate.yml` runs the focused crate tests and release
  policy check. It uploads the complete crate-test log for seven days and adds
  compiler/test-failure context (up to 3000 characters) to both a failure
  annotation and the step summary.
- `.github/workflows/nodemigrate-integration.yml` runs shell syntax validation
  on relevant pull requests.
- Manual `workflow_dispatch` starts isolated K3s and upstream Kubernetes
  lanes. Each lane builds the combined runtime binary and standalone
  `nodemigrate` binary as test setup, then runs
  `.github/scripts/nodemigrate-integration.sh` as root and uploads its log.
  The kubeadm source setup installs `crictl` from the matching cri-tools minor
  release (override with `CRI_TOOLS_VERSION`) and records the resolved tool
  version for static-pod cleanup diagnostics.
- Manual dispatch also starts the Docker five-node preflight. It builds
  `.github/nodemigrate/five-node.Dockerfile`, then
  `.github/scripts/nodemigrate-docker-preflight.sh` starts five disposable
  systemd containers and checks namespace separation, containerd CRI, a loaded
  eBPF classifier, per-node volumes, inter-node reachability, and single-node
  failure isolation. The preflight has not been dispatched; it does not yet
  provision Kubernetes or test Cilium datapath or migration parity.
- The script installs the selected upstream distribution first, uses Cilium
  with Flannel disabled in the K3s lane, installs the hostPath CSI test
  driver, cert-manager, Traefik, nginx, static and CSI-backed claims, then
  checks the source → nodestore → retained-source round trip. Every checkpoint
  asserts the Cilium DaemonSet rollout and CiliumEndpoint CRD are present.
- Every checkpoint writes a private canonical snapshot of important behavior
  and a fingerprint inventory for every listable API object nodemigrate is
  expected to transfer. Fingerprints use the export sanitizer's same treatment
  of status, API-assigned identity fields, transient kinds, and service-account
  token Secrets; live NodeMetrics and PodMetrics samples are omitted. Secret
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
- Dedicated migration workflow: do not dispatch until the user authorizes
  migration runtime/e2e execution. Its compile step is test setup, not a
  generic build gate.
- Static shell syntax, formatting, diff whitespace, and docs-link checks are
  allowed and must be reported separately from runtime evidence.

## Verification log

The latest completed targeted nodemigrate check is `ad509e5b` (run
`36031941620`): crate tests, packaging, and crate detection passed. The
integration workflow's pull-request validation also passed in run
`36031941811`, including `bash -n` for the new Docker preflight. The preflight
and migration jobs were skipped on the pull request. No local Cargo test/build
was run.

| Date | SHA | Check/lane | Result | Evidence |
| --- | --- | --- | --- | --- |
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
| — | — | K3s + Cilium round trip | Not run; manual dispatch pending authorization | — |
| — | — | Upstream Kubernetes + Cilium round trip | Not run; manual dispatch pending authorization | — |
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
| — | — | Migration state and metadata fixtures | Integration script fingerprints all exported API objects, checks Node label/annotation/taint and ConfigMap annotation at every stage, and compares ConfigMap data hashes on return. The focused crate tests now pass. Migration round-trip and five-node runtime evidence remain pending. | — |

For every new result, record the commit SHA, workflow run URL, lane, resolved
Kubernetes/K3s/Cilium/add-on versions, and pass/fail state at each checkpoint.
Keep failures and skipped checks visible rather than replacing them with a
later green run.
