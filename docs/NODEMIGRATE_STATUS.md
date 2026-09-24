# nodemigrate status dashboard

Last updated: 2026-09-24

This dashboard tracks the full nodemigrate goal in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). Detailed status is kept in the
separate living documents below.

## Current state

| Area | State | Detail |
| --- | --- | --- |
| Full bidirectional migration implementation | In progress; replacement uses UID-preconditioned Node deletes and preserves Node labels, annotations, taints, and schedulability. Forward exports carry node scheduling metadata for later offline control-plane or worker joins after source API quorum is lost. Reverse staged quorum orchestration and the five-node coordinator remain incomplete. | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| Existing nodestore member replacement | Replacement ordering and Raft learner catch-up guard implemented; focused nodemigrate, nodebootstrap, and nodestore checks passed at `c468627045cae98360bed8a93397407ff934ed86`; runtime scenario unverified | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| Worker-node migration | K3s agent, upstream kubelet, and not-k8s nodelet roles are inventoried. Forward and return worker paths avoid cluster-wide re-import, guard stale same-name Node replacement, preserve local PV data, and wait for fresh Ready registration. Runtime behavior remains unverified. | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| K3s external-CNI uninstall preservation | Detects the configured CNI directories, passes them through to containerd on the target, and snapshots/restores directories under K3s's data path around explicit uninstall. Focused checks passed; real K3s+Cilium uninstall behavior remains unverified. | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| K3s/Cilium and upstream Kubernetes/Cilium test lanes | Latest migration run passed full source checks and displayed the unattended risk warning in both lanes. K3s still chose flannel because its implicit default bypassed active Cilium detection; active-provider selection now takes precedence. Upstream reached nodeapiserver startup but port 6443 remained bound. Focused Tokio-context regression passes. | [CI status](NODEMIGRATE_CI_STATUS.md) |
| Nodemigrate merge gates | Both mandatory gates remain not run: one-node K3s+Cilium round trip with no returned-state differences, and a 3-control-plane + 2-worker upstream round trip with joined replacement and no returned-state differences. Five-node isolation and runtime evidence are still required. | [CI status](NODEMIGRATE_CI_STATUS.md) |
| Docker five-node isolation preflight | The image built, but the first container exited with code 255 before systemd readiness without container output. The entrypoint now prints the resolved systemd binary and enables debug console logs while retaining private cgroup namespaces. Isolation, CRI, BPF, network, and failure/restart remain unverified. | [CI status](NODEMIGRATE_CI_STATUS.md) |
| Standalone artifact and shared-version behavior | Release design present; nodemigrate publication not recorded | [Release status](NODEMIGRATE_RELEASE_STATUS.md) |

## Current verification

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

1. Keep both real-cluster lanes and the existing-cluster join/replacement case
   marked unverified until their authorized runtime checks pass.
2. Run the Docker capability preflight when migration-runtime execution is
   authorized; if it passes, build the five-node kubeadm scenario and staged
   control-plane coordinator.
3. Track release readiness and publication separately; do not bump the shared
   version for nodemigrate-only publication.
