# nodemigrate CI and integration status

Last updated: 2026-09-24

This is the living CI record for the scope in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). The user-specific testing policy
is recorded there and overrides conflicting general `AGENTS.md` gates for
this objective.

## Workflow

- `.github/workflows/nodemigrate-integration.yml` runs shell syntax validation
  on relevant pull requests.
- Manual `workflow_dispatch` starts isolated K3s and upstream Kubernetes
  lanes. Each lane builds the combined runtime binary and standalone
  `nodemigrate` binary as test setup, then runs
  `.github/scripts/nodemigrate-integration.sh` as root and uploads its log.
  The kubeadm source setup installs `crictl` from the matching cri-tools minor
  release (override with `CRI_TOOLS_VERSION`) and records the resolved tool
  version for static-pod cleanup diagnostics.
- The script installs the selected upstream distribution first, uses Cilium
  with Flannel disabled in the K3s lane, installs the hostPath CSI test
  driver, cert-manager, Traefik, nginx, static and CSI-backed claims, then
  checks the source → nodestore → retained-source round trip. Every checkpoint
  asserts the Cilium DaemonSet rollout and CiliumEndpoint CRD are present.
- Every checkpoint now writes a private canonical snapshot of node names and
  roles, workload and ingress specs, PV/PVC bindings, add-on deployments,
  Cilium daemonset images and readiness, required CRD schemas, and
  storage-class configuration. The returned snapshot must match the initial
  one. The migration Certificate secret is compared by digest, without writing
  secret data to logs or checkpoint files.
- Runtime coverage for joining an existing cluster and replacing a member is
  a separate required scenario; the current script does not establish that
  case as verified.

## Required merge gates

Nodemigrate needs both isolated round trips green before merge:

- Single-node K3s with Cilium → not-k8s → K3s, with no differences in the
  checked cluster state, workload behavior, add-ons, ingress, certificates,
  persistent data, or Cilium health between initial and returned checkpoints.
- Upstream Kubernetes with three control-plane nodes and two worker nodes →
  not-k8s → upstream Kubernetes, with all node membership and the same
  workload, add-on, ingress, certificate, persistent-data, and Cilium checks.
  Include the existing-cluster join/replacement path.

QEMU or another isolation mechanism may host the cluster lanes on one CI node.
Docker remains a candidate only after the test setup proves it models the
networking, node identity, service management, storage, and failure behavior
under test. Record the simulator and its limitations with the results. The
current integration workflow does not yet implement or pass these full merge
gates.

## Multi-node implementation gap

The present script runs source, not-k8s, and return stages on one host OS. It
does not provision five independently isolated nodes. The migration code also
does not yet coordinate reverse control-plane cutover: the original stacked
etcd quorum must be brought up across at least two retained control planes,
while the not-k8s Raft membership must remain available for API export and be
retired safely as each control plane leaves. Repeating the current single-host
script or running it once per node would not satisfy the five-node gate.

Next, add an isolated five-node provisioner and a staged control-plane
cutover/return protocol, then validate Docker’s network namespaces, systemd,
CRI, per-node storage, Cilium behavior, and node failure isolation in that
same topology. Keep QEMU as the alternative if Docker fails any of those
checks.

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
| — | — | Canonical initial/returned state comparison | Implemented in the integration script; `bash -n` and jq filter checks passed locally. GitHub shell validation and runtime evidence pending. | — |

For every new result, record the commit SHA, workflow run URL, lane, resolved
Kubernetes/K3s/Cilium/add-on versions, and pass/fail state at each checkpoint.
Keep failures and skipped checks visible rather than replacing them with a
later green run.
