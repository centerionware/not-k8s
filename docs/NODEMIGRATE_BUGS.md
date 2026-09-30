# nodemigrate bug and fix tracker

Last updated: 2026-09-30

## Migration matrix 36778028681 failures

Branch-built matrix
[36778028681](https://github.com/centerionware/not-k8s/actions/runs/36778028681)
passed the upstream Kubernetes round trip and failed its K3s and Docker
five-node lanes. Logs are saved in `/tmp/nodemigrate-36778028681-job-*.log`.

- **Component: `nodemigrate` K3s CRI/Cilium recovery.** Return migration
  exhausted two 60-second `StopPodSandbox` attempts for a local workload
  sandbox after rebuilding Cilium state. The CRI returned `DeadlineExceeded`
  and `RST_STREAM ... CANCEL`. Rollback restored nodestore; its final workload
  checkpoint and Pod-origin API ClusterIP probe passed. A snapshot during
  recovery showed transient no-route errors and `KubeletNotReady` which cleared
  before that checkpoint. The worktree now inspects the CRI sandbox after a
  StopPodSandbox deadline and proceeds only if it reports `SANDBOX_NOTREADY`;
  its focused `nodemigrate` quick-check passed at SHA `d923d555` in
  [36785639656](https://github.com/centerionware/not-k8s/actions/runs/36785639656).
- **Component: `nodemigrate` staged kubeadm control-plane return.** The
  five-node run staged all three retained control planes, then protected API
  import failed waiting for `cp-1` to become Ready. The captured log lacks its
  Node conditions and kubelet/Cilium diagnostics. The current worktree keeps
  the staged Node UID instead of deleting/re-registering it before import and
  includes condition details in readiness timeout errors. Its first quick-check
  caught a missing test-module import, now fixed. Follow-up `nodemigrate`
  quick-check passed at SHA `3d353277` in
  [36785046986](https://github.com/centerionware/not-k8s/actions/runs/36785046986).
- **Component: `nodemigrate` upstream recovery.** The Kubernetes single-node
  migration round trip passed with the recent bounded CRI retries, Cilium
  Envoy restart, and standalone Pod recreation changes. This validates that
  lane only; it does not close the K3s or multi-control-plane failures.

## Migration matrix 36770365252 failures

Branch-built matrix
[36770365252](https://github.com/centerionware/not-k8s/actions/runs/36770365252)
passed the nodemigrate and combined-runtime builds in all lanes, then exposed
three recovery issues. Full logs and artifacts are in
`/tmp/nodemigrate-36770365252-job-*.log` and
`/tmp/nodemigrate-36770365252-artifacts/`.

- **Component: `nodemigrate` CRI cleanup transitions.** K3s return migration
  failed on a `StopPodSandbox` `DeadlineExceeded`; the five-node Docker
  preflight failed removing a CSI attacher container while containerd said it
  was still starting. The candidate retries those specific cleanup states with
  bounded attempts; other CRI errors remain immediate failures. Focused CI and
  a new migration matrix are pending.
- **Component: `nodemigrate` Cilium Envoy recovery.** Upstream Kubernetes
  return migration rebuilt the Cilium agent and Socket LB links but left the
  host-network `cilium-envoy` DaemonSet Pod running. The saved diagnostics show
  repeated Envoy readiness/liveness 503s and the Cilium agent failing to reach
  `/var/run/cilium/envoy/sockets/admin.sock`. The candidate restarts only the
  local DaemonSet-owned Envoy Pod and waits for its replacement to stay Ready.
  Focused CI and migration verification are pending.
- **Component: `nodemigrate` standalone Pod recovery.** In that same upstream
  return lane, sandbox removal left the live `migration-standalone` Pod in
  `ContainerStatusUnknown`. Since it has `restartPolicy: Never` and no
  controller owner, kubelet does not create a replacement Pod after its
  sandbox disappears. The candidate recreates such live ownerless Pods with a
  fresh UID after their local CRI sandboxes are removed. Focused CI and
  migration verification are pending.
- **Component: upstream workload recovery — unresolved symptom.** CoreDNS
  logged an invalid/expired ServiceAccount token and lost API Service access in
  the same return lane. The evidence does not yet prove whether Cilium Envoy
  recovery, token refresh, or another migration effect caused this. Recheck
  CoreDNS token authentication and API Service reachability in the next
  migration run; do not treat the candidate fixes as resolving this symptom.

## Migration matrix 36764002800 failures

Branch-built matrix
[36764002800](https://github.com/centerionware/not-k8s/actions/runs/36764002800)
failed all three lanes after its utility and combined-runtime builds passed.
The logs are saved under `/tmp/nodemigrate-36764002800-artifacts/` and
`/tmp/nodemigrate-36764002800-{docker,k3s,kubernetes}-job.log`.

- **Component: `nodemigrate` local Pod-agent selection.** Upstream return
  cleanup failed removing a running container. The system had an installed
  upstream kubelet configured for the same CRI endpoint, but that service was
  inactive; its mere presence suppressed Nodelet fallback. The candidate now
  falls back to active Nodelet when no active matching upstream agent control
  was selected. A focused regression covers that fallback predicate.
- **Component: `nodemigrate` K3s process identification.** K3s return cleanup
  rejected `/usr/local/bin/k3s server` as not the K3s executable because the
  code inspected mutable `/proc/<pid>/cmdline`. The candidate reads
  `/proc/<pid>/exe` and verifies the executable basename, with a focused
  predicate regression.
- **Component: Nodelet probe target refresh.** At the five-node nodestore
  checkpoint, Nodelet's timeout messages show CoreDNS probes targeting
  `10.42.0.11` and cert-manager probes targeting `10.42.0.71`, while the
  current Pods had IPs `10.42.0.103` and `10.42.0.91`. Cilium listed the local
  CoreDNS endpoints Ready. The probe supervisor was keyed only by Pod name and
  retained its original IP after CNI reassigned addresses. The candidate
  stores the supervisor target IP and replaces its tasks when that IP changes;
  a focused regression covers the changed-target case.

The `nodelet,nodemigrate` quick-check passed at SHA
`b501aa682ebc5edb0ebec99d34c23c5c83f789cf` in
[36769436388](https://github.com/centerionware/not-k8s/actions/runs/36769436388).
Migration runtime verification is still required. No general build or full e2e
ran for 36764002800.

## Confirmed Cilium clean-state service-datapath failure

Focused upstream K3s+Cilium 1.20.2 KPR diagnostic
[36716154527](https://github.com/centerionware/not-k8s/actions/runs/36716154527)
failed at SHA `4761372698dd661ddd95419fbd19cdb0707b5064` after 19m27s (job
`109889475497`). This run did not invoke nodemigrate. The baseline fixture,
including source host-origin CoreDNS probes, passed. Cilium's
`clean-cilium-state` reset removed the ordinary workload endpoint; the
expected reachability-loss probes timed out. The diagnostic then deleted the
CoreDNS Pod through the API. K3s created a new Pod/IP, Cilium observed the CNI
ADD and logged successful endpoint creation, but CoreDNS stayed unready for
more than five minutes because its Kubernetes plugin could not reach
`https://10.43.0.1:443`. CoreDNS readiness logs report the Kubernetes plugin
not ready; Cilium's BPF service map marked the Kubernetes ClusterIP
non-routable despite its service listing an active backend. This confirms
that recreating a workload endpoint alone does not restore the Kubernetes
Service datapath after this clean-state reset. Cilium documents
`clean-cilium-state` as an invasive all-state cleanup whose BPF state must be
reconstructed from Kubernetes; see [Cilium configuration](https://docs.cilium.io/en/stable/network/kubernetes/configuration/).
Follow-up no-migration diagnostic
[36719510988](https://github.com/centerionware/not-k8s/actions/runs/36719510988)
confirmed that a second Cilium agent restart with the clean-state flag
restored did become Ready and its final BPF service map showed a backend for
`10.43.0.1:443` at `10.1.0.10:6443`. The replacement CoreDNS Pod's host
`/health` request succeeded, but `/ready` returned 503 and its API watches
failed. The run timed out waiting for CoreDNS before its Pod-origin TCP probe.
The reordered diagnostic
[36722537619](https://github.com/centerionware/not-k8s/actions/runs/36722537619)
then established that a generic Pod's TCP connection to the Kubernetes API
ClusterIP `10.43.0.1:443` times out, while direct access to its backend
`10.1.0.50:6443` succeeds. The second Cilium agent restart was Ready, and
service listing/BPF map showed an active backend, so the failure is in the
ClusterIP translation path rather than API backend reachability or CNI
creation. Focused diagnostic
[36725184073](https://github.com/centerionware/not-k8s/actions/runs/36725184073)
then captured the failed flow. A fresh diagnostic Pod got IP `10.42.0.162`,
and Cilium logged successful endpoint creation and BPF program reload. Its
SYN to `10.43.0.1:443` was observed `to stack`; the service map still listed
active backend `10.1.0.160:6443`, and the direct-backend probe passed. Cilium
also dropped replies addressed to old CoreDNS IP `10.42.0.139` as `Stale or
unroutable IP`; this stale traffic is separate from the diagnostic Pod's
`.162` flow and is not established as its cause. The failing ClusterIP flow
was observed at the endpoint-to-stack handoff. Later mapped-link inspection
found that the pinned Socket LB links survived Cilium's cleanup/restart with
targets pointing at the old cgroup hierarchy; the controlled unpin-and-agent
reattach recovery is recorded below. These diagnostics did not invoke
nodemigrate; its implementation still requires focused CI before migration is
retried.

Inspection of the exact upstream Cilium `v1.20.2` source confirms its
`clean-cilium-state` init calls `cilium-dbg post-uninstall-cleanup --all-state`;
that cleanup detaches Socket LB cgroup programs and unmounts the configured
cgroup root before the agent starts ([cleanup implementation](https://github.com/cilium/cilium/blob/v1.20.2/cilium-dbg/cmd/post_uninstall_cleanup.go),
[Socket LB cgroup attachment](https://github.com/cilium/cilium/blob/v1.20.2/pkg/socketlb/cgroup.go)).
The first attachment inspection showed Socket LB programs on the cgroup root
immediately after cleanup, so their absence alone does not explain the initial
route failure. The later ordinary Cilium restart did leave the configured
cgroup root mounted at `/../../../..` with no Socket LB programs attached there.

The controlled recovery experiment
[36744527476](https://github.com/centerionware/not-k8s/actions/runs/36744527476)
removed only the pinned `cil_sock*` Socket LB links and restarted the Cilium
agent. The replacement links targeted the active cgroup root (ID `1`) and the
API ClusterIP probe passed. This verifies the recovery operation in the
no-migration K3s+Cilium fixture. `nodemigrate` now performs the same targeted
link unpin, starts a replacement agent with `clean-cilium-state` temporarily
disabled, restores the original ConfigMap value after its init starts, waits
for stable readiness, and then recreates local non-host-network Pod sandboxes.
Focused nodemigrate quick-check [36748080605](https://github.com/centerionware/not-k8s/actions/runs/36748080605)
at SHA `30627f9720b2d9196f2bf9918b981156479117c6` caught an `E0716`
temporary-lifetime compile error in the replacement-Pod list request. SHA
`72845a8d762e94a8cf0d8de7f00949c7a9aa50f5` binds a named `ListParams`, and
focused nodemigrate quick-check [36748550052](https://github.com/centerionware/not-k8s/actions/runs/36748550052)
passed. PR script validation [36748961972](https://github.com/centerionware/not-k8s/actions/runs/36748961972)
also passed shell, fixture, and diagnostic checks on SHA
`ad0d734d7755d9dff998272b0c942b3d961a9615`. Migration remains unverified and
should not be retried while the other known migration blockers remain open.

Run 36728443584 did not reach the cgroup capture: immediately after Cilium
clean-state, the probe treated a temporary lack of a Running CoreDNS Pod as a
failure. The run diagnostics show the replacement CoreDNS Pod was created at
14:37:38 and became Running at 14:37:44, two seconds after the probe exited.
This is a confirmed fixture timing defect, not evidence about Socket LB or
ClusterIP behavior. The expected-unreachable probe now accepts that transient
absence, and the cgroup attachment capture runs before probing CoreDNS.
Focused local shell/checker validation passed, and follow-up
[36731098394](https://github.com/centerionware/not-k8s/actions/runs/36731098394)
at SHA `da6c3ee2f3044a0ae905596fb8b70afabea5f2ef` got past that fixture race.
at SHA `da6c3ee2f3044a0ae905596fb8b70afabea5f2ef` showed the Socket LB programs
attached at the cgroup root after cleanup, but absent from the configured root
after the second Cilium agent restart. It could not determine when the Service
route first failed.

The stage comparison in
[36733704748](https://github.com/centerionware/not-k8s/actions/runs/36733704748)
at SHA `2b315ece2beaccb181402c9129aef0df6457af4a` found the Pod-origin API
ClusterIP probe already failing immediately after `clean-cilium-state`, before
the second restart. The direct API backend probe passed and the Cilium service
map listed that backend active. At this point the Cilium endpoint table still
showed existing endpoints `regenerating`; the fresh CoreDNS endpoint later
showed `waiting-to-regenerate`. Therefore the probe ran before complete endpoint
recovery, and does not establish a persistent post-recovery route failure. The
second restart checkpoint also failed and had no Socket LB programs attached
at the configured cgroup root. Run
[36736279009](https://github.com/centerionware/not-k8s/actions/runs/36736279009)
confirmed the failure persists after a 60-second retry: Cilium restored its
endpoints and loaded the probe endpoint's BPF program, but API ClusterIP SYNs
remained untranslated. The service map showed an active API backend, and direct
TCP to that backend succeeded.

Pinned-link follow-up
[36738848742](https://github.com/centerionware/not-k8s/actions/runs/36738848742)
identified a strong cgroup attachment regression. Before cleanup, all Socket
LB links targeted cgroup ID `1` while the configured cgroup mount root showed
`/../../../..`; the baseline Pod-origin ClusterIP probe passed. After
`clean-cilium-state`, the links targeted cgroup ID `23507`, the mount root
showed `/`, and the Pod-origin probe failed. After the ordinary second agent
restart, the mount root returned to `/../../../..`, but link target `23507`
persisted and the route remained broken. Thus cgroup ID `23507` is now the
leading cause, though its location relative to the host and workload cgroups
still needs direct mapping. The mapping follow-up is recorded below.

Run [36741671607](https://github.com/centerionware/not-k8s/actions/runs/36741671607)
resolved cgroup ID `23751` to `/sys/fs/cgroup` in the hierarchy visible to the
post-cleanup Cilium Pod. Its pinned Socket LB links and cgroup mount root both
had ID `23751`; after an ordinary Cilium Pod restart, the mount root returned
to ID `1` but the links still targeted `23751`, which no longer resolved in the
visible hierarchy. Both the Cilium agent and probe reported `0::/`, so this
does not prove their host ancestry. The evidence does show pinned links target
a cgroup no longer visible after restart. Cilium's `v1.20.2` `attachCgroup`
updates an existing pinned link in place and only unpins it when update returns
`ENOLINK` ([source](https://github.com/cilium/cilium/blob/v1.20.2/pkg/socketlb/cgroup.go#L70-L105)); this is consistent with the agent's `Updated link`
logs while link ID `23751` persisted. The next isolated experiment unpins the
stale links and restarts the agent, then checks whether it attaches to the
current root and restores Service routing. Migration remains disabled. Logs:
`/tmp/nodemigrate-36741671607-k3s-probe-job.log` and
`/tmp/nodemigrate-36741671607-artifact/nodemigrate-k3s-cilium-restart-36741671607/nodemigrate-k3s-cilium-restart.log`.

Full logs for the preceding run:
`/tmp/nodemigrate-36733704748-k3s-probe-job.log` and
`/tmp/nodemigrate-36733704748-artifact/nodemigrate-k3s-cilium-restart-36733704748/nodemigrate-k3s-cilium-restart.log`.

Full logs are saved at
`/tmp/nodemigrate-36716154527-k3s-probe-job.log` and
`/tmp/nodemigrate-36716154527-artifact/nodemigrate-k3s-cilium-restart.log`.
Run 36725184073 logs are saved at
`/tmp/nodemigrate-36725184073-k3s-probe-job.log` and
`/tmp/nodemigrate-36725184073-artifact/nodemigrate-k3s-cilium-restart.log`.
The diagnostic runs emitted repeated K3s probe-manager "already exists"
messages during the unready interval; these are observed secondary symptoms,
not yet established as an independent cause.

## Additional confirmed diagnostic fixture defect

Restart-only run
[36706135439](https://github.com/centerionware/not-k8s/actions/runs/36706135439)
failed before installing K3s. Its root-run workflow step did not preserve
`GH_TOKEN` for the script's `gh release download`; captured output says
`gh auth login` is required, `SOURCE_KUBECONFIG` remained unset, and K3s was
not installed. No Cilium or workload check ran. The workflow now passes the
GitHub token through `sudo`; rerun this diagnostic path before drawing any
conclusion from it.

Corrected run
[36707412491](https://github.com/centerionware/not-k8s/actions/runs/36707412491)
passed ordinary K3s+Cilium KPR agent restart, Cilium sandbox handoff, K3s
restart, same-name Node replacement, API resource, CoreDNS, API ClusterIP,
CSI, StatefulSet, Job, RBAC, and workload checks. It did not enable
`clean-cilium-state` and used K3s's kubelet. This narrows the remaining probe
failure to the migration cleanup and/or Nodelet path, but does not identify
which one. The no-migration diagnostic
[36710560344](https://github.com/centerionware/not-k8s/actions/runs/36710560344)
completed with a harness defect. Cilium's operator replaced CoreDNS, but the
probe used the old Pod IP; the script then manually deleted CRI sandboxes under
K3s's kubelet, which retained stale probe state and failed to recover CoreDNS.
Corrected follow-up run
[36713423587](https://github.com/centerionware/not-k8s/actions/runs/36713423587)
completed after setup and the `notk8s` build passed. Cilium's clean-state init
and replacement agent were healthy, but host probes to the current CoreDNS
UID/IP timed out. The endpoint table contained only host and health endpoints.
This confirms that the reset removed the ordinary Pod endpoint while its CRI
sandbox remained. The run did not remove CRI state manually or invoke
nodemigrate. The worktree now tests recovery after an API-managed CoreDNS Pod
replacement triggers fresh CNI setup.

## Confirmed no-migration diagnostic harness defect

Run [36710560344](https://github.com/centerionware/not-k8s/actions/runs/36710560344)
completed at SHA `72ddff65`. The `clean-cilium-state` init exited successfully
and Cilium remained Ready. The first post-cleanup host probe targeted the old
CoreDNS Pod IP after the operator had replaced that Pod. The diagnostic then
removed 17 non-host-network Pod sandboxes and 24 container records directly
through CRI while K3s's kubelet still owned the API Pods. Kubelet logged stale
probe-manager entries, and CoreDNS did not recover within five minutes. This
result does not diagnose Nodelet's CRI cleanup or migration behavior. The
diagnostic is being corrected to wait for a Ready replacement CoreDNS Pod and
probe its current IP, without deleting CRI state under Kubelet.

PR validation [36713356086](https://github.com/centerionware/not-k8s/actions/runs/36713356086)
then failed because its lightweight checker still called the removed ordinary
Cilium-restart helper. The checker now exercises Ready-CoreDNS filtering and
expected reachability loss, and guards against direct CRI cleanup under
Kubelet. The local diagnostic checker passes. PR validations
[36713670774](https://github.com/centerionware/not-k8s/actions/runs/36713670774)
and [36714065919](https://github.com/centerionware/not-k8s/actions/runs/36714065919)
passed. API-managed CoreDNS CNI-recovery diagnostic
[36716154527](https://github.com/centerionware/not-k8s/actions/runs/36716154527)
failed on SHA `47613726`; PR validation
[36716148475](https://github.com/centerionware/not-k8s/actions/runs/36716148475)
passed on that commit.

## Latest migration result: 36700106403

Run [36700106403](https://github.com/centerionware/not-k8s/actions/runs/36700106403)
is terminal at SHA `c77dc6fdc08bd67568f74091945ee4b5a544ccd9`. The dedicated
branch builds passed; all three runtime lanes failed.

- **Component: Nodelet/Cilium host-to-Pod probe path — unresolved.** Upstream
  Kubernetes job `109837299287` and five-node job `109837299640` both failed at
  the `nodestore` checkpoint while waiting for CoreDNS rollout. Nodelet logged
  HTTP readiness/liveness timeouts to CoreDNS Pod IPs on the migrated node.
  The five-node cluster still had Ready CoreDNS endpoints on other nodes, and
  Cilium listed the local endpoints as Ready. Upstream audit records show
  CoreDNS's ServiceAccount successfully watching Services, EndpointSlices, and
  Namespaces, so token auth and API access were working. The exact local
  host-to-Pod datapath cause is not yet proven. `/run/xtables.lock` did not
  appear in this run's failures.
- **Component: migration fixture Job verification — confirmed test defect.**
  K3s job `109837299550` completed forward and return migration, passed the
  returned-stage Node/Cilium/CoreDNS/API/CSI/StatefulSet checks, then failed
  because `kubectl logs job/migration-job` no longer contained the original
  output. The Pod remained `Succeeded` and its Job remained `Complete`, but
  migration had removed its terminal CRI sandbox. Runtime logs are not durable
  Kubernetes API state. The current fixture verifies the retained Job/Pod
  terminal status and launches a fresh per-stage Job for execution checks.
  Shell, fixture, and diagnostic validation passed in
  [36748961972](https://github.com/centerionware/not-k8s/actions/runs/36748961972),
  but the updated assertions still need a live migration round trip.

The fixture assertion correction passed static validation and still needs a
live round trip. Do not retry migration until the separately observed Nodelet
versus CRI sandbox-removal race is fixed and its focused nodemigrate quick-check
passes; a repeated matrix run is not a diagnostic.

Migration run
[36690929745](https://github.com/centerionware/not-k8s/actions/runs/36690929745)
has completed with all three lanes failed. **Component: nodemigrate CRI
cleanup.** Both single-node jobs captured Nodelet removing a sandbox/container
between nodemigrate's CRI list and `rm`/`rmp`; treating explicit `NotFound` as
success makes the cleanup idempotent while connection, permission, and other
errors remain fatal. A focused classifier regression is now in the worktree.
**Component: nodemigrate rollback endpoint selection.** K3s rollback captured
destination Cilium sandbox IDs from `/run/containerd/containerd.sock` but
used `NODEMIGRATE_CRI_ENDPOINT`, which still named the stopped source K3s
runtime. Control-plane and worker rollback now carry the destination endpoint
explicitly. Both fixes passed the focused `nodemigrate` quick-check at
[36696369212](https://github.com/centerionware/not-k8s/actions/runs/36696369212)
on SHA `548e31a4`.

**Component: Docker five-node hostpath CSI recovery.** All five migration
operations completed and the lane reached the nodestore CSI restore step, but
the imported `csi-hostpathplugin` StatefulSet rollout timed out after the
test patched its durable state volume. Final diagnostics show Ready Nodes,
CRI tasks running, but the plugin and many workload Pods Unknown. All five
CoreDNS Pods were Running with Pod IPs but `0/1` Ready. That left
`wait_for_coredns()` closed on every Nodelet, including worker-1, so ordinary
Pod reconciliation remained paused and the CSI Pod stayed Unknown. The CSI
rollout timeout is downstream of this gate. The saved journal records one
CoreDNS liveness restart, but not the readiness probe's connection result or
HTTP status. Nodelet now emits failure-only probe target and error details
without changing readiness or restart decisions. Its focused quick-check
passed at SHA `b4647a9c` in
[36698119270](https://github.com/centerionware/not-k8s/actions/runs/36698119270)
(job `109830878708`). The actual CoreDNS probe failure is still unresolved;
wait for actionable evidence before another migration run. The worker kubelet
RBAC and missing Pod/Node errors in the same artifact occurred before each
worker joined the target; source kubelet traffic was still reaching the
replacement API during handoff, so do not treat those lines as a confirmed
target authorization defect. Captured evidence is under
`/tmp/nodemigrate-36690929745*` and is already available locally; do not fetch
it again. The remaining CoreDNS failure needs diagnosis before migration
verification continues.

Open PR #591 remains on `feat/nodemigrate-migration`; the latest implemented
code commit covered here is `b4647a9c9c0bd001d53c8bef1821d4bb484e5a76`.
Nodelet quick-check passed on code SHA `b4647a9c`
in [36698119270](https://github.com/centerionware/not-k8s/actions/runs/36698119270).
Migration-specific crate/packaging checks passed on PR revision `2c4bf903`
[36698894529](https://github.com/centerionware/not-k8s/actions/runs/36698894529),
and nodemigrate tests passed in
[36698894559](https://github.com/centerionware/not-k8s/actions/runs/36698894559).
Neither workflow exercised a live migration. Focused quick-check
[36673247341](https://github.com/centerionware/not-k8s/actions/runs/36673247341)
failed while compiling `nodemigrate`; Nodelet passed 390 non-CRI and 1216
CRI-enabled tests. The complete job log is
`/tmp/nodemigrate-quick-check-36673247341.log`. Rust found a `String`/`str`
comparison mismatch and the cleanup wrapper accepted an already-completed
`Result` as a future, plus the test needed an explicit result type. These
compile issues were fixed in `678a0bba`; follow-up quick-check
[36673743053](https://github.com/centerionware/not-k8s/actions/runs/36673743053)
passed all `nodemigrate` tests. The Nodelet source is unchanged from `f8bfc025`,
where both suites passed. Push-triggered PR validation
[36673741337](https://github.com/centerionware/not-k8s/actions/runs/36673741337)
passed shell syntax, jq snapshots, Helm-state, API-inventory, and diagnostic
JSON checks; all live migration jobs were skipped. `nodemigrate checks`
[36673741370](https://github.com/centerionware/not-k8s/actions/runs/36673741370)
passed crate tests and packaging policy; release build/publish were skipped.
Migration workflow
[36674138076](https://github.com/centerionware/not-k8s/actions/runs/36674138076)
completed at SHA `678a0bba` with `runtime_source=branch`, Cilium KPR, and the
five-node lane enabled. K3s passed its round trip. Upstream safely rolled back
after a Cilium readiness deadline, and the five-node fixture failed before
nodemigrate ran. Commit `f514e61a` addresses the fixture failure and adds
Nodelet diagnostics for the still-unconfirmed upstream CRI file-mount error.

Migration run
[36684339965](https://github.com/centerionware/not-k8s/actions/runs/36684339965)
failed in all lanes. K3s and upstream reached nodestore, then their CoreDNS
Pods failed readiness. Scheduler events report binding conflicts with an
empty requested resourceVersion. The nodeapiserver binding path treated an
explicitly empty value as a stale precondition; the fix now treats it as
omitted and adds focused tests. The upstream artifact also shows
nodelet sandbox recreation rejected by CRI name reservations held by earlier
sandbox IDs, despite nodemigrate reporting its sandbox-removal pass. Docker
completed the forward nodestore checks and reverse migrations, but its
returned-cluster CSI rollout timed out. The final snapshot showed CSI and
workload Pods Unknown while Nodes were Ready. Earlier `cni plugin not
initialized` reports on cp-2 preceded Cilium readiness and are not confirmed as
the final Docker cause. The Docker artifact also records the same empty-version
Binding conflicts on CoreDNS and Cilium DNS Service backends in `maintenance`
state. Its CSI/Pod-status symptom is consistent with that Binding failure, but
restoration remains unverified until a migration rerun. The worktree cleanup
now stops each selected sandbox, removes its child CRI containers, then removes
the sandbox so the runtime can release its generated name. Focused quick-check
for `nodeapiserver,nodemigrate` passed at `5da7ade0` in
[36689888324](https://github.com/centerionware/not-k8s/actions/runs/36689888324).
The follow-up migration run is now active as
[36690929745](https://github.com/centerionware/not-k8s/actions/runs/36690929745)
with branch-built runtime, Cilium KPR, and five-node migration enabled.
Artifacts from the previous run remain in
`/tmp/nodemigrate-36684339965-artifacts/`.

- Cilium cleanup now tracks the replacement Pod UID, cleanup-init exit code,
  and a continuous Ready interval for that same Pod. Each loop checks its
  deadline, bounds the replacement-Pod list request by remaining time, emits
  bounded state changes, and captures Pod, container-log, and event details
  on deadline. The clean-state ConfigMap restoration is attempted after both
  success and operation failure, with resourceVersion protection. These
  changes and their focused state tests passed nodemigrate checks in
  [36677830069](https://github.com/centerionware/not-k8s/actions/runs/36677830069); the Cilium
  environment value is read from a ConfigMap key reference when its container
  starts, so the actual replacement init/CRI sequence still needs runtime
  evidence.
- Nodelet now wakes Pods waiting for a missing dynamic CSI driver when that
  driver registers, bounds and coalesces retries by Pod UID, and retries
  failed CSINode/Node topology metadata reconciliation. Retry cancellation
  signals tasks without aborting an in-flight CRI mutation, then revalidates
  the current Pod UID before publishing retry status. Focused tests are added
  and passed the Nodelet quick-check in
  [36677863272](https://github.com/centerionware/not-k8s/actions/runs/36677863272).
- The Docker five-node fixture pins the hostpath CSI plugin to the PV topology
  node before provisioning fixture claims, checks driver readiness on all
  required nodes, seeds its node-specific topology label before provisioning,
  resolves PV topology values back to Nodes, and preserves existing StatefulSet
  selectors. Script validation passed in
  [36677829876](https://github.com/centerionware/not-k8s/actions/runs/36677829876).

Nodelet now treats an empty `subPath` as the volume root, adds the container
name to CRI create/start errors, and logs Cilium agent mount source/target paths
when container creation fails. Nodelet quick-check passed in
[36677863272](https://github.com/centerionware/not-k8s/actions/runs/36677863272).
Run 36674138076 repeatedly reported
`failed to stat "/run/xtables.lock/": ... not a directory` while Cilium's
replacement remained Pending. The official Cilium manifest declares
`/run/xtables.lock` as a `FileOrCreate` hostPath with a `/run/xtables.lock`
mount target and no `subPath`; therefore the saved run does not prove the new
empty-subPath handling is the cause. The added failure-only mount diagnostics
will show the exact input if this persists. Nodemigrate crate and packaging
checks passed in
[36677830069](https://github.com/centerionware/not-k8s/actions/runs/36677830069).
Branch-runtime migration run
[36678250408](https://github.com/centerionware/not-k8s/actions/runs/36678250408)
completed at SHA `f514e61a` with Cilium KPR and the five-node lane enabled.
K3s passed its full round trip in 26m (job `109767861015`). The upstream
Kubernetes lane completed forward and return migration, then failed returned
workload recovery: Pod-to-Service API requests to `10.96.0.1:443` returned
`no route to host`, and the hostpath CSI readiness PVC remained Pending. Cilium
reported its replacement agent Ready and listed the API Service backend as
active, but its endpoint snapshot showed only five ready endpoints while the
cluster had many more Pods. The migration reset had cleared Cilium's local
endpoint state without forcing extant workload sandboxes through CNI ADD
again. The worktree now removes only the local Node's non-host-network CRI Pod
sandboxes after Cilium is Ready, preserving API objects and host-network
control-plane sandboxes so their owning kubelet/runtime recreates Pod network
state. This replaces the previous `/run/xtables.lock/` failure hypothesis;
that error did not recur. The Docker five-node lane passed migration to
nodestore and all
workload checks through that stage, then its fixture exited because the
separate checkpoint process had not initialized `NODEMIGRATE_KUBECTL_IMAGE`.
The working tree now derives that image in every fixture process and checks
Pod-to-Service API routing immediately after return, before CSI setup can
obscure that signal. The fixture image and Cilium reset fixes need focused
validation before another migration run. Captured logs/artifacts are in
`/tmp/nodemigrate-36678250408-{k3s,kubernetes,docker}.log` and
`/tmp/nodemigrate-36678250408-artifacts/`.

## Findings from migration run 36664092690

Run [36664092690](https://github.com/centerionware/not-k8s/actions/runs/36664092690)
tested SHA `18520d8ddb360e01bf2aa665c65cce22390762fb` with branch-built
components, Cilium KPR, and the five-node path. The K3s forward/return round
trip passed in 25m21s. Full logs are saved under
`/tmp/nodemigrate-36664092690-{kubernetes,docker}.log` and downloaded
artifacts under `/tmp/nodemigrate-36664092690-artifacts/`.

- **Component: nodemigrate Cilium cleanup wait.** The upstream job ended when
  its 60-minute migration step timeout fired, not because GitHub or the user
  canceled it. The log stopped after the replacement Cilium Pod appeared.
  Inspection found the five-minute deadline check and sleep after an
  unconditional `loop`, making both unreachable; the Rust compiler emitted an
  `unreachable statement` warning. The loop now checks the deadline and sleeps
  on each unsuccessful iteration. Focused nodemigrate checks are pending.
- **Component: five-node hostpath CSI placement.** Docker preflight reached
  workload checks, but `migration-stateful-0` stayed Pending: both fixture PVs
  were topologically pinned to `worker-1` while the imported
  `csi-hostpathplugin-0` ran on `worker-2`. Nodelet on worker-1 consequently
  could not find a local hostpath CSI driver. The five-node fixture now pins
  the CSI StatefulSet to the fixture PV topology node at source, nodestore, and
  returned checkpoints. The source setting is captured for migration and
  reasserted after fixture reinstall. Shell validation and migration-specific
  verification are pending.
- **Timeout interpretation.** The earlier 30-minute whole-job limit was
  insufficient: the healthy K3s round trip alone took 25m21s, before setup,
  builds, and final checks. The current single-node job allows 90 minutes and
  the migration step 60 minutes. The upstream lane hit the 60-minute step cap
  because of the confirmed infinite loop above; timeout increases alone would
  only conceal that defect.

No general e2e or regular build gate ran. Do not retry the migration matrix
until the focused fixes pass.

## Findings from migration run 36656072014

Run [36656072014](https://github.com/centerionware/not-k8s/actions/runs/36656072014)
tested SHA `346fb0f7d16267f97c772ce0d872cb8de05522c9` with branch runtime,
Cilium KPR, and the five-node migration path. Full lane logs are saved under
`/tmp/nodemigrate-36656072014-{kubernetes,k3s,docker}.log`.

- **Component: nodeapiserver Pod binding conflict cause.** The upstream lane
  recorded 233 Binding HTTP 409 responses across 24 Pods. The persistent
  `migration-stateful-0` binding retried every 11 seconds, remained unbound,
  and the target API emitted no `Pod binding storage retries exhausted`
  warning. This rules against internal CAS retry exhaustion as the observed
  cause, but audit metadata does not include the Binding request body, so it
  cannot distinguish stale resourceVersion from UID, deletion, or already-
  bound preconditions. The old 409 body incorrectly said `delete precondition
  failed` for every cause. The current branch returns a specific Conflict
  message for each precondition without dropping UID/resourceVersion checks.
  Inspection confirms the active Rust scheduler sends JSON Binding requests
  containing the Pod UID, so the earlier protobuf-decoding hypothesis does
  not explain this run; its test-only UID edit was reverted. A likely
  scheduler-side cause is same-name Pod replacement leaving the old UID in
  the scheduling queue. The watch path now removes all old-UID state before
  routing a replacement and ignores stale Deletes when updating the mirror.
  The deterministic replacement/relist regression and `nodescheduler`
  quick-check passed at SHA `18520d8ddb360e01bf2aa665c65cce22390762fb` in
  [run 36663804244](https://github.com/centerionware/not-k8s/actions/runs/36663804244).
  Migration run [36664092690](https://github.com/centerionware/not-k8s/actions/runs/36664092690)
  is testing whether this mechanism accounts for the observed 409s; it is
  plausible but is not yet tied to every conflict in the prior run.
  Focused `nodeapiserver` quick-check passed at SHA
  `ab4d1778ebed1f948e17614140b9a771543e3813` in
  [run 36661272533](https://github.com/centerionware/not-k8s/actions/runs/36661272533).
  The stale-UID mechanism is plausible but has not yet been tied to every
  409 recorded by this run. Migration runtime verification remains pending
  until all encountered runtime issues have actionable fixes and focused
  checks pass.
- **Component: returned Cilium readiness during five-node workload checks.**
  The Docker five-node probe passed cluster bring-up and reached post-migration
  workload checks. On worker-2, a Cilium CNI ADD for `migration-daemon` ended
  with `signal: killed`. Nodelet logged a 30-second reconcile timeout. At the
  same time, the replacement Cilium Pod's `clean-cilium-state` init container
  exited unsuccessfully, was retried, and its `cilium-agent` container started
  after the failed CNI ADD. The final diagnostic snapshot shows the Pod
  `1/1 Running`, so this CNI failure recovered and was not the terminal lane
  failure. Logs do not reveal why the init container failed or whether the CNI
  process received a cancellation or host signal. Continue checking CNI
  readiness on each returning node.
- **Component: K3s return fixture timing.** K3s completed forward and return
  migration and most returned-stage checks, but the overall GitHub job hit its
  30-minute timeout while Traefik reported 0/1 available. The job ran from
  01:38:28Z to 02:08:45Z; the migration step began at 01:49:42Z and was stopped
  after about 19 minutes, before its own 30-minute timeout or the rollout
  command's five-minute deadline. Run metadata has `cancelled_by: null`. This
  is a job-timeout truncation, not evidence of a Traefik regression. The job
  timeout was raised to 60 minutes in commit `ab4d1778` and is now 90 minutes;
  the migration step is now capped at 60 minutes. This lane remains incomplete
  and must be rerun after the actionable runtime issues are resolved.

- **Component: five-node CSI fixture patch race.** The Docker lane failed while
  adding the Nodelet target-root mount to the imported `csi-hostpathplugin`
  StatefulSet. Its controller wrote status between the fixture's GET and
  PATCH; nodeapiserver returned HTTP 409 for this optimistic-concurrency race.
  The script now retries this exact conflict by invoking `kubectl patch` again,
  which rereads the current StatefulSet. Other errors still fail immediately;
  eight repeated conflicts fail the step. Shell validation and runtime
  verification are pending.

The run's job IDs were Docker 109700594144, K3s 109700594250, and upstream
Kubernetes 109700594260. Its five-node build/preflight succeeded before the
workload failure. No general e2e or regular build gate ran.

## Findings from migration run 36650426292

Run [36650426292](https://github.com/centerionware/not-k8s/actions/runs/36650426292)
tested SHA `a8c791328a91976f76557d80701cea4a115d3bd1` with branch-built
components, Cilium KPR, and the five-node migration path. Its completed job
metadata shows K3s passed, upstream Kubernetes failed during the nodestore
fixture, and Docker five-node failed during the return fixture. Logs are saved
under `/tmp/nodemigrate-36650426292-{kubernetes,k3s,docker}.log`.

- **Component: nodeapiserver Pod binding under concurrent writes.** Forward
  migration completed, but the imported hostpath CSI Pod remained unscheduled.
  The scheduler saw 236 binding HTTP 409 responses across 24 Pods. The binding
  handler validates caller preconditions against fresh state, then persists
  with a compare-and-swap; a concurrent status/metadata update can make that
  storage CAS conflict even when the caller's UID/resourceVersion precondition
  is still valid. This is the likely cause based on the repeated 409s and the
  handler path, not explicit conflict telemetry. The branch now retries only
  that internal storage conflict by re-reading and recomputing the binding;
  caller precondition conflicts still return immediately. Focused
  `nodeapiserver,nodelet` quick-check passed at SHA `f6e7e4d6` in
  [run 36655430632](https://github.com/centerionware/not-k8s/actions/runs/36655430632);
  migration runtime verification is pending.
- **Component: Nodelet CRI handoff from kubelet-owned containers.** The
  five-node lane passed its preflight and nodestore checkpoint, then the
  returned `migration-standalone` Pod remained Unknown. Containerd reported
  the expected CRI container name reserved by a still-running container from
  the old kubelet sandbox. The branch's lookup, status, and resource snapshot
  paths now recognize standard `io.kubernetes.container.name` and
  `io.kubernetes.container.type` labels alongside Nodelet's labels, so it can
  find existing kubelet attempts rather than creating a duplicate. Focused
  `nodelet` quick-check passed in run 36655430632; a five-node migration rerun
  is pending.
- **Timeout scope.** The Docker step is an aggregate five-node scenario with
  multiple migrations. Each individual nodemigrate process is capped at 30
  minutes; the aggregate step retains a larger window to finish all node and
  return checkpoints. Single-node migration steps and their enclosing jobs are
  both capped at 30 minutes, so setup/build time is included in the bound.
  Focused
  `nodeapiserver,nodelet` quick-check passed at SHA `f6e7e4d6`
  in [run 36655430632](https://github.com/centerionware/not-k8s/actions/runs/36655430632).
  The migration-specific rerun can now validate both runtime fixes.

The first focused quick-check for the fixes, [36654928410](https://github.com/centerionware/not-k8s/actions/runs/36654928410)
at SHA `36ec70fab426b160a057cdfef78322ea94dad5ce`, passed the non-CRI nodelet
tests (388 tests) but stopped compiling the CRI-enabled nodelet because the
new label helper returns `&str` while the resource snapshot owns its name.
The snapshot now converts that borrowed name to an owned `String`. The job
stopped before running `nodeapiserver`; replacement focused quick-check passed
for both crates at SHA `f6e7e4d6` in
[run 36655430632](https://github.com/centerionware/not-k8s/actions/runs/36655430632).

## Findings from migration run 36644181073

Run [36644181073](https://github.com/centerionware/not-k8s/actions/runs/36644181073)
tested SHA `81cd6b4c631106f4ca0d0572a7f125969dbb13df` with branch-built
components, Cilium KPR, and the five-node lane. All three lanes failed; full
logs are saved once under `/tmp/nodemigrate-36644181073-*.log`.

- **Component: nodemigrate Cilium handoff / upstream Kubernetes lane.** The
  ServiceAccount token refresh and protected import both returned, and Tokio
  runtime shutdown returned immediately. The lane then made no visible
  progress before the 30-minute migration cap. In forward migration, the next
  operation after import is Cilium host-state cleanup. The current branch adds
  boundaries around import, Cilium cleanup, and source rollback, and bounds
  each Cilium API request to 30 seconds so an unanswered API call cannot
  overrun the cleanup retry deadline. The suspected cleanup stall is not
  confirmed yet; the migration lane must verify success or bounded failure.
  The first quick-check for the request bounds failed to compile because the
  async requests borrowed temporary Kubernetes parameter objects. Those
  parameters now live through each awaited request. The corrected focused
  quick-check passed at SHA `3d152cab6ad1f92eb2eb6be78d9742870f334a12` in
  [run 36650152894](https://github.com/centerionware/not-k8s/actions/runs/36650152894).
- **Component: returned K3s Cilium/CSI path.** Forward and return migration
  completed, Cilium's replacement agent remained Ready, and the returned K3s
  audit and preserved CSI volume assertion passed. Reinstalling the upstream
  hostpath CSI fixture then left its readiness PVC Pending; API requests to
  the Kubernetes Service IP timed out. The captured Cilium service listing
  showed an active API backend, while the BPF load-balancer map had the API
  Service without a routable backend entry at diagnostic time. This points to
  a Cilium datapath restoration gap, but its precise cause remains unverified.
  Returned K3s now resets Cilium before K3s re-registers its Node, restoring
  the ordering from the prior passing K3s round trip. Other retained targets
  still reset after the replacement Node is Ready, as required by the earlier
  upstream Kubernetes Traefik failure. Both paths require another migration
  check before the ordering is considered verified.
- **Component: five-node timeout scope.** The Docker job used one 30-minute
  step limit for the entire five-node migration sequence. It completed cp-1
  and cp-2 migrations successfully before the overall step expired, so this
  did not prove that an individual migration exceeded 30 minutes. The script
  now bounds each nodemigrate invocation to 30 minutes while the aggregate
  Docker step keeps a 180-minute window for the multi-node sequence. This
  applies to future workflow runs; an already-dispatched run retains its
  original step limit.

The `nodebootstrap,nodemigrate` quick-check passed for the run's SHA in
[run 36643843760](https://github.com/centerionware/not-k8s/actions/runs/36643843760).
After the follow-up timeout, ordering, diagnostic, and bounded-request changes,
the focused `nodemigrate` quick-check passed at SHA
`3d152cab6ad1f92eb2eb6be78d9742870f334a12` in
[run 36650152894](https://github.com/centerionware/not-k8s/actions/runs/36650152894).
PR validation also passed. No regular build or general e2e gate ran. The next
migration-specific run is now eligible; it must verify the upstream recovery
and returned K3s datapath paths.

## Findings from migration run 36633722194

Run [36633722194](https://github.com/centerionware/not-k8s/actions/runs/36633722194)
tested SHA `c2e9a470919f84f11b05d6f21a09f00d4a380e86` with branch-built
components, Cilium KPR, and the five-node lane. The run was cancelled after the
upstream Kubernetes lane made no visible progress for about 57 minutes after
reissuing its migrated ServiceAccount token. The Docker artifact was retrieved
to `/tmp/nodemigrate-36633722194-docker-artifact/`.

- **Component: nodemigrate import completion.** `refresh_service_account_token_secrets`
  logged that it refreshed the token, but the caller never logged that API
  import completed. The capture shows no further importer log during the next
  57 minutes. The importer now logs immediately before and after its
  `block_on` boundary and uses `Runtime::shutdown_timeout(5s)` before returning,
  which bounds Tokio blocking-task teardown. The boundary logs will distinguish
  a future that never returned from a runtime shutdown stall. The mechanism is
  still unverified; quick-check and a migration rerun are required.
- **Component: Cilium Service datapath after K3s return.** The migration itself
  completed in both directions, and the replacement Cilium agent became Ready.
  After return, the K3s API Service at `10.43.0.1:443` did not serve requests;
  metrics-server and other API clients logged timeouts/unauthenticated
  requests. Agent Pod readiness did not prove the Service datapath. The
  post-reset check needs to exercise API Service connectivity before migration
  is called healthy. Exact underlying Cilium datapath cause remains unverified.
- **Component: worker Nodelet client-certificate trust.** The Docker artifact
  shows the `pods/log` request was rejected with HTTP 401 after the preceding
  Pod GET succeeded. `nodeapiserver` authenticates its kubelet-proxy requests
  with a client certificate when configured; Nodelet only accepts that
  identity when `NODELET_CLIENT_CA_FILE` is set. Bootstrap previously set this
  only for control planes, leaving workers to use a bearer-token fallback that
  this proxy does not forward. Worker Nodelet setup now extracts the active
  cluster CA from the supplied kubeconfig (embedded data or a relative/absolute
  CA file), writes it as a public local trust file, and configures Nodelet to
  use it. An explicit `NODELET_CLIENT_CA_FILE` still takes precedence. Focused
  tests and CI are pending.
- **Component: five-node CSI fixture and scheduler placement.** Worker-2's
  Nodelet repeatedly failed the hostpath CSI `NodePublishVolume` call because
  the target path's parent did not exist in the CSI driver's view. The
  stateful Pod was scheduled on worker-2 while the imported CSI StatefulSet
  plugin Pod was on worker-1. The PV evidence shows both fixture CSI volumes
  carry the `topology.hostpath.csi/node=worker-2` affinity. The integration
  fixture now pins its single-node hostpath driver to the common topology node
  before recording the source checkpoint, keeping the node-local volume and
  driver together through import. Script validation and a five-node run are
  pending.

The existing Cilium probe exercised Pod-to-API-Service connectivity at source
and post-restart points but not during each `verify_stage`. It now runs from
inside a Pod after Node, Cilium, and CoreDNS readiness on every source,
nodestore, and returned-source verification stage. This directly checks the
Service datapath that failed on the previous K3s return.

The migration execution steps are now capped at 30 minutes. Prior healthy
single-node round-trip jobs took about 24–25 minutes including builds; the
longer enclosing job limits remain available for setup, compilation, log
capture, and artifact upload. No rerun has started while these failures remain
unresolved.

## Findings from migration run 36627336634

- **Component: nodemigrate Cilium return ordering.** K3s passed. Upstream
  Kubernetes completed forward and return migration and storage checks, but
  returned Traefik could not pass `/ping` probes. Logs show a healthy API
  Service backend after Cilium cleanup, then retained Node replacement and a
  Cilium local Node deletion. The ordering is a plausible cause, not a proven
  diagnosis. Cilium reset now runs after the replacement Node is Ready; a
  migration rerun must verify the Traefik datapath.
- **Component: Nodelet CRI cleanup.** Docker five-node timed out waiting for
  `migration-standalone`. Worker-1 logs show its CNI ADD canceled after the
  30-second Nodelet reconcile timeout, then CRI refused a container name as
  reserved by a previous container for the same Pod UID. Nodelet now removes
  orphaned containers by exact nodelet/Kubernetes Pod UID labels before reusing
  or creating a sandbox, excluding the selected sandbox. Focused CI and Docker
  migration verification remain pending. No OOM evidence was found.

## Findings from migration rerun 36621933601

The `nodelet` quick-check passed at SHA
`d88206ef7a8ddd4783e635b4f322e3b42f7c3ea6` in
[run 36621353742](https://github.com/centerionware/not-k8s/actions/runs/36621353742).
The migration-specific run
[36621933601](https://github.com/centerionware/not-k8s/actions/runs/36621933601)
then passed bidirectional migration, workload, storage, and parity assertions
for both K3s and upstream Kubernetes with Cilium KPR enabled. The five-node
Docker-backed kubeadm/Cilium lane reached post-migration workloads, with
CoreDNS Running, but timed out waiting for the hostpath CSI StatefulSet. Its
Pod stayed `Unknown` on worker-2 because containerd rejected the CSI container's
bidirectional `/var/lib/nodelet` mount: the simulated node's `/` mount was
private. The Docker node entrypoint now makes `/` recursively shared and
verifies the propagation mode before starting systemd. The five-node rerun is
pending. The earlier single-node parity and resolver failures are corrected
in the tested SHA; the shared-mount fixture correction still needs live evidence.

- **Component:** five-node Docker node fixture. `systemd-entrypoint.sh` now
  establishes and asserts recursive shared propagation on `/`, so CSI's
  bidirectional HostPath mounts behave like the intended host setup. Shell
  validation is pending.

## Findings from migration run 36614282579

At SHA `6d91d1df7f3837dca583d359e3aba76030634500`, forward migration and
storage checks passed in both single-node lanes. Each then stopped at strict
API parity on the same test-only CSI change. The five-node lane migrated three
control planes and joined one worker before the imported HostPath CSI
StatefulSet remained unready on cp-1.

- **Component:** migration integration fixture. The target-only
  `nodemigrate-nodelet-root` hostPath mount changed five CSI StatefulSet spec
  fields after the source snapshot. The parity failure was correct; the fixture
  now installs and verifies that exact mount on the source before capturing its
  baseline. The existing target setup is idempotent, so full spec comparison
  remains enabled. Shell validation is pending; no migration retry has run.
- **Component:** `nodelet` resolver handoff / five-node CSI recovery. Run
  36614282579 timed out during `kubectl rollout status
  statefulset/csi-hostpathplugin` after cp-1 migration. The kubeadm source had
  `resolvConf: /etc/kubernetes/nodemigrate-resolv.conf`, but Nodelet ignored the
  retained KubeletConfiguration and read the Docker node's `/etc/resolv.conf`.
  CoreDNS then detected a resolver loop and crash-looped; its Pods remained
  unready and Nodelet's startup gate held imported CSI Pods in `Unknown`. Nodelet
  now prefers the retained kubelet `resolvConf` path and has focused parser
  regressions. The `nodelet` quick-check and five-node runtime recheck are
  pending.

The `nodeapiserver,nodelet,nodestore` quick-check passed at the preceding code
SHA `30b0894778bac62be3187f68ac8f839470313a34` in
[run 36613539517](https://github.com/centerionware/not-k8s/actions/runs/36613539517).
Both fixes are in the current worktree; focused `nodelet` and migration-script
validation are pending, and no migration retry has run.

## Findings from migration run 36608846633

At SHA `5f64d13dc932c88c722325eaa79abc16628f7222`, all branch binary builds
passed, but K3s, upstream Kubernetes, and five-node Docker round trips failed.
Saved logs are under `/tmp/nodemigrate-36608846633/`.

- **Component:** `nodeapiserver` scheduler Binding decoding. Audit records show
  repeated HTTP 400 responses for scheduler `pods/binding` requests. Inspection
  found the protobuf virtual-resource decoder was accidentally wired to Pod
  eviction while the actual Binding path still decoded JSON. Binding now uses
  the `Binding` protobuf schema and captures Content-Type before consuming the
  body; eviction now decodes its own `Eviction` schema.
- **Component:** migration CSI fixture. K3s's imported HostPath CSI driver
  returned `NodePublishVolume` errors because its container could not see
  `/var/lib/nodelet/pods/...`, where nodelet creates target paths. The fixture
  now mounts `/var/lib/nodelet` into the CSI hostpath container with bidirectional
  propagation and checks that mount after rollout.
- **Component:** `nodestore` learner promotion. Docker cp-3 became Ready and
  Cilium passed its clean-state/readiness check, then promotion raced the
  learner's final replicated entry (`matched=2600`, leader tail `2601`). The
  membership RPC now retries only this explicit not-caught-up result for at
  most 15 seconds; Raft still checks activity and the log tail before proposing
  promotion.
- **Component:** `nodelet` CRI sandbox discovery. K3s logs include one CoreDNS
  sandbox-name reservation during recovery; later diagnostics show the same
  Pod scheduled successfully. The CRI lookup now checks source and nodelet label
  sets together and falls back to exact sandbox metadata namespace/name when
  legacy CRI labels are absent. This is a defensive correction; the run's
  terminal K3s failure was CSI target-path visibility, not this recovered
  reservation.
- The worker kubelet RBAC and Node-not-found errors in Docker were logged after
  cp-3 promotion failed and rollback began. They are recovery symptoms after
  the destination API was stopped, not evidence of an independent pre-failure
  worker authorization defect.

The `nodeapiserver,nodelet,nodestore` quick-check passed at the corrected SHA
`30b0894778bac62be3187f68ac8f839470313a34` in run
[36613539517](https://github.com/centerionware/not-k8s/actions/runs/36613539517).
The migration-specific rerun is now eligible. No regular build or full e2e
gate ran.

## Findings from migration run 36603273571

At SHA `862e48a771b5c645f35594b70cfc5019941f1317`, the branch-built
`notk8s` runtime and standalone `nodemigrate` compiled in all three lanes, but
the K3s, upstream Kubernetes, and five-node Docker migrations failed. The
complete logs were saved once under `/tmp/nodemigrate-36603273571/`.

- **Component:** `nodemigrate` Cilium join ordering. The five-node lane waited
  for a Cilium agent on cp-2 before installing the replacement node agent, so
  the destination had no Node for the DaemonSet to match. Cilium state reset
  now runs after the joined control-plane Node becomes Ready, followed by a
  second readiness check. Single-node migration keeps its existing reset
  timing. Targeted component CI and migration rerun are pending.
- **Component:** `nodelet` CSI plugin discovery. K3s and upstream showed
  repeated `no CSI driver configured` warnings for `hostpath.csi.k8s.io` and
  the imported StatefulSet remained Pending. The hostpath registrar continued
  using `/var/lib/kubelet/plugins_registry`, while nodelet watched only
  `/var/lib/nodelet/plugins_registry`. When nodelet uses its default registry
  path it now also watches the source kubelet registry directory; an explicit
  registry-path override remains exclusive. A focused directory-discovery
  regression was added. Targeted CI and migration rerun are pending.
- **Component:** `nodelet` CRI sandbox discovery. K3s logs show kubelet-created
  sandboxes reserving names for Pods that nodelet was recreating. The previous
  lookup used only `nodelet.dev` labels and could not see those source
  sandboxes. When no nodelet-labeled sandbox matches, lookup now falls back to
  Kubernetes CRI namespace/name labels and uses sandbox metadata UID to drive
  the existing stale cleanup. Targeted CI and migration rerun are pending.

The additional scheduler `volume node affinity conflict` messages occurred
while the replacement Node was not yet registered and cleared after node
registration; they were not the final CSI failure. No migration retry has
started after these combined corrections.

## Findings from migration run 36591891203

- **Component:** `nodeapiserver` Pod binding wire format. At tested SHA
  `98b3429599c88d855b407f25639f038b96ede1cb`, the upstream Kubernetes lane
  completed forward migration and then failed redeploying HostPath CSI. Target
  Pods remained unscheduled and the API audit log recorded HTTP 400 responses
  for scheduler `pods/binding` requests. The binding handler decoded every
  body as JSON although Kubernetes clients may send protobuf `Binding`
  envelopes. It now uses the shared virtual-resource decoder and has a focused
  protobuf Binding regression. Targeted CI and a migration rerun are pending.
- **Component:** `nodestore` Raft learner join. On the five-node Docker lane,
  cp-1 migrated successfully and added cp-2 as a learner. A fresh cp-2
  `RawNode` started with no voters, then stopped while applying the leader's
  committed `AddLearner` entry: `applying conf change: removed all voters`.
  The member now seeds its empty local ConfState with the live peer probe's
  existing voters, while still requiring the committed log entry to add itself
  as a learner. A focused state-selection regression and targeted CI are
  pending. The TLS BadCertificate and later kubelet/API errors were follow-on
  symptoms after nodestore stopped.
- **Component:** five-node migration diagnostics. The orchestration captured
  each cp/worker `nodemigrate` output in a command substitution and discarded
  it on failure, leaving only a generic join error. It now prints the captured
  utility output and exit status before failing.

The user-approved migration-specific run is
[36591891203](https://github.com/centerionware/not-k8s/actions/runs/36591891203)
at the SHA above (`runtime_source=branch`, `cilium_kpr=true`,
`five_node_migration=true`). K3s passed both migration directions and its
parity/workload/Cilium/RBAC/PV checks. Upstream Kubernetes passed forward
migration but failed the post-forward HostPath CSI scheduling check. Docker
preflight passed, cp-1 passed forward migration, and cp-2 failed joining. No
retry has been launched after these failures.

The first targeted quick-check after these fixes,
[36596720100](https://github.com/centerionware/not-k8s/actions/runs/36596720100)
at SHA `2f60975d888e8fc6f449a094543bc2812301fc13`, failed compilation because
the binding handler read `Content-Type` after consuming the request body. The
handler now saves that header before body decoding. No migration retry has
been launched.

The corrected `nodeapiserver,nodestore` quick-check
[36597286663](https://github.com/centerionware/not-k8s/actions/runs/36597286663)
passed at SHA `1fb881ab441a24f214678ebfaab2317c75b972f1`. The migration-specific
round-trip retry is now eligible; no regular build or full e2e gate ran.

## Findings from migration run 36582910964

- **Component:** Docker five-node orchestration. At SHA
  `9d6fab17f856899b4885cfb20309dd2571ce6706`, the Docker preflight passed and
  cp-1 successfully migrated to nodestore, imported all 55 CRDs, passed
  destination readiness, and rebuilt Cilium host state. The script then
  failed copying cp-1's `/etc/nodebootstrap/admin.kubeconfig` to cp-2 because
  `/etc/nodebootstrap` did not exist there. The shared `copy_file` helper now
  creates the target parent before copying; shell syntax validation and a
  full migration retry are pending. This is an orchestration/fixture bug, not
  a confirmed nodemigrate runtime failure. Log evidence is in
  `/tmp/nodemigrate-36582910964-artifact/nodemigrate-docker-preflight.log`.
  Run: [36582910964](https://github.com/centerionware/not-k8s/actions/runs/36582910964),
  Docker job `109455502184`; artifact `nodemigrate-docker-preflight-36582910964`.
- **Component:** Docker preflight environment. Preflight-only run
  [36590354974](https://github.com/centerionware/not-k8s/actions/runs/36590354974)
  passed in 6m24s with Cilium KPR enabled and migration disabled, confirming
  the isolated kubeadm 3-control-plane/2-worker setup and Docker checks pass.
  It did not exercise the full migration or the new kubeconfig-copy fix.
- **Component:** `nodemigrate` CRI cleanup correction. The 60-second CRI
  sandbox cleanup timeout and agent-last Cilium sandbox ordering passed the
  K3s and upstream migration round trips at the same SHA. This does not verify
  the five-node kubeadm migration, whose Docker job failed before a successful
  result was captured.

## Findings from migration run 36578066781

- **Component:** `nodemigrate` CRI cleanup. The K3s and upstream single-node
  migrations completed forward and return with semantic state, API objects,
  Cilium, and persistent data checks passing. The five-node kubeadm migration
  passed topology, Cilium KPR, storage, and source workload checks, then failed
  while removing a stopped Cilium Envoy sandbox. Containerd logged successful
  StopPodSandbox/network teardown, but `crictl rmp` returned DeadlineExceeded at
  its default two-second request timeout. Cleanup now gives CRI sandbox stop
  and removal 60 seconds, and removes non-agent Cilium sandboxes before the
  local Cilium agent so CNI teardown can still reach it. The failure handler
  restarted source services; the diagnostic snapshot captured kubelet active
  but the local API still refusing connections immediately afterward, so full
  source API recovery is not yet established. Logs: `/tmp/nodemigrate-36578066781/`.
- **Component:** Docker migration verification. This earlier run's isolated
  three-control-plane and two-worker cluster passed its source checks, but
  migration failed during Cilium sandbox cleanup; run 36582910964 now passes
  both single-node round trips with that correction, while its Docker failure
  cause awaits artifact retrieval.
- Single-node K3s and upstream round trips passed at SHA
  `092099a1f8e62fc25c161d70e0501bfe3d192aa0`; Docker migration failed at that
  same SHA. Do not retry the migration until the CRI cleanup correction and
  scoped validation are pushed together.
- **Component:** `nodemigrate` cleanup-order regression. Focused quick-check
  [36582345104](https://github.com/centerionware/not-k8s/actions/runs/36582345104)
  at `37dcc70e2bac26e971075cceba686e12609ea9aa` ran 115 tests; 114 passed.
  The new assertion incorrectly reordered Envoy and operator IDs even though
  the implementation preserves CRI order among non-agent sandboxes. The test
  now expects Envoy, operator, then agent; migration has not been retried.
  Corrected nodemigrate-only quick-check
  [36582679193](https://github.com/centerionware/not-k8s/actions/runs/36582679193)
  passed all 115 tests at SHA
  `603d8f1d31ee47d5625c677e1459691bb1e3ec26`.

## Findings from migration run 36573504554

- **Component:** Migration fixture parity. At SHA
  `8cc222a1fe492f9e6caa760ad752387448f8b2e6`, the upstream Kubernetes lane
  completed forward migration and passed nodestore workload, storage/PV,
  Cilium KPR, API trust, RBAC/token, and CRD `/status` checks. Parity then
  rejected the regenerated CiliumNode Cilium address and IPAM podCIDR. These
  are Cilium-owned per-node allocations; the snapshot normalizer now excludes
  those exact generated fields while the live Cilium/networking assertions
  remain strict. The return path remains unverified.
- **Component:** Migration fixture parity. K3s completed the return migration
  and all returned-stage workload/storage/API assertions, then the round-trip
  comparison rejected the replaced node's `kube-system/<node>.node-password.k3s`
  Secret `data.hash`. K3s rotates this hash when a Node UID is replaced. The
  comparator now permits only that named Secret and only `/data/hash`; all
  other API object fields remain strict. The workflow also logged one
  metrics-server timeout through the API ClusterIP on its first startup; its
  rollout and subsequent returned-stage checks passed, so the logs show
  recovery rather than a persistent datapath failure.
- **Component:** Five-node migration fixture. Docker preflight built an isolated
  kubeadm 3-control-plane/2-worker Cilium KPR cluster, passed its control-plane
  loss/recovery and isolation checks, then fixture apply hit a transient
  ResourceQuota resourceVersion conflict while its controller updated status.
  No nodemigrate operation ran in this lane. Fixture setup now retries the
  idempotent manifest apply only when the API reports that specific conflict,
  up to five times; other failures remain fatal.
- Full job logs and uploaded artifacts are preserved once under
  `/tmp/nodemigrate-36573504554/`. Local `bash -n`, focused Cilium filter
  validation, and `git diff --check` pass. The consolidated corrections still
  need scoped CI before another migration attempt.

## Findings from migration run 36556999686

- **Component:** `nodelet` CRI cgroup configuration. The five-node Docker lane
  failed on cp-1 when runc rejected `/k8s.io/<container-id>` under
  `SystemdCgroup=true`. The previous change set the QoS parent on
  `RunPodSandbox`, but containerd also reads the repeated sandbox config on
  `CreateContainer`; nodelet left that copy empty. The CreateContainer request
  now receives the same QoS parent. A focused regression covers the repeated
  config; scoped CI and migration verification are pending.
- **Component:** `nodemigrate` Cilium recovery. On K3s return, the replacement
  agent's `clean-cilium-state` init exited 1. K3s logged that init in
  `CrashLoopBackOff` and reported a container-removal race; nodemigrate
  returned on the first failed attempt before kubelet's retry could run. The
  utility now lets kubelet retry within its existing bounded readiness wait,
  captures each failed attempt's termination details and last 80 log lines,
  and reports the last failure if recovery does not complete. The underlying
  reason for the initial exit remains unverified.
- **Component:** `nodemigrate` Cilium readiness / API restore ordering. The
  upstream return reset reported success, but a later cluster snapshot found
  the replacement Cilium agent and Envoy unready, the Node without Cilium
  networking, CoreDNS reporting invalid/expired bearer tokens and then no
  route to the API Service IP, and cert-manager's webhook with no ready
  endpoints. Certificate, CertificateRequest, and ClusterIssuer restoration
  then failed admission calls. This supports a shared Cilium/service-network
  readiness failure; the exact token and datapath mechanism remains unproven.
  Cilium must now remain Ready for ten continuous seconds before API objects
  are restored. On nodestore-source cutover, nodemigrate also stops/removes
  only CRI sandboxes carrying the `nodelet.dev/pod-uid` ownership label after
  stopping nodelet, allowing the destination kubelet to recreate Pods and
  request credentials from its own API. It preserves all API objects and PV
  payloads so rollback can reconcile workloads. These changes and a stable
  return datapath remain unverified.

## Findings from migration run 36552342215

- **Component:** `nodebootstrap` migration PKI. On SHA
  `a91fc102b553248dd697d17f97896f6b0f84680f`, the K3s lane reached source
  shutdown and failed because K3s's default serving CA key is SEC1/P-256 PEM,
  which the new migration signer passed directly to rcgen's PKCS#8 parser.
  The migration signer now applies the same SEC1-to-PKCS#8 conversion already
  used by `nodecontroller`'s CSR signer.
- **Component:** `nodemigrate` rollback. After that bootstrap failure, rollback
  tried to list partial destination Cilium pods using the source K3s CRI socket,
  which was correctly stopped as part of cutover. Rollback now inspects the
  destination's `/run/containerd/containerd.sock` only when its containerd
  service and socket are present, then stops partial destination services and
  restores the source. The observed source restore was blocked by this cleanup
  error. Both K3s fixes passed `nodebootstrap,nodemigrate` quick-check
  [36554687444](https://github.com/centerionware/not-k8s/actions/runs/36554687444)
  at SHA `9e33da42ea9fd408d1551a089f73dd7b425e00d6`.
- **Component:** `nodemigrate` upstream return activation. The completed
  Kubernetes lane migrated forward, then return activation started kubelet
  before the retained source containerd service. Kubelet could not connect to
  `/run/containerd/containerd.sock` and the return API stayed unavailable.
  `service::activate` now restores the retained runtime before kubelet.
- **Component:** `nodemigrate` reverse rollback. On the same lane, rollback
  queried Cilium CRI state after the target runtime/socket was gone, preventing
  the partial target from being stopped and leaving nodestore stopped. Cleanup
  now skips CRI and static-pod operations when the runtime is unavailable and
  stops an available target runtime as part of rollback.
- **Component:** `nodelet` CRI cgroup placement. The five-node lane reached
  replacement Cilium on `cp-1`, where the `config` init container failed with
  runc reporting `expected cgroupsPath ... slice:prefix:name ... got
  "/k8s.io/<container-id>"`; containerd had `SystemdCgroup=true`. The source
  log does not show the `RunPodSandbox` cgroup parent, so it does not prove
  whether the parent was omitted or transformed incorrectly. Nodelet now
  passes only the QoS parent (the runtime owns the per-pod child) and logs that
  CRI parent at sandbox creation. This targets the observed malformed path;
  the exact failure has not yet been re-exercised.
- The run is complete and failed in all three lanes. The K3s PKI and forward
  rollback fixes passed focused CI. The upstream return/rollback and nodelet
  cgroup corrections remain unvalidated pending the current migration run.

## Previous migration run 36556999686

Migration run
[36556999686](https://github.com/centerionware/not-k8s/actions/runs/36556999686)
completed with failures at SHA `457a7f6305ea6ebc2317a2e100d3c3b0a23b7fd2`.
Its three confirmed failure mechanisms and their fixes are documented above.
The later migration result 36573504554 is the current verification record;
do not use this older run as evidence for the latest changes.

## Findings from migration run 36546160178

- **Component:** Migration integration fixture. At SHA
  `0bc38d03fe43c53864a0d108716818513fc2b200`, both single-node lanes completed
  CRD import, API readiness, StatefulSet/PV/data checks, RBAC/token checks, and
  CRD `/status` checks at `stage=nodestore`. Both then failed because the
  fixture used `kubectl logs -c` for an ephemeral container; the API rejected
  that logs request even though the ephemeral container was Running. The probe
  now writes its marker to a shared `emptyDir`, which the regular probe
  container reads with `kubectl exec`. `bash -n` and `git diff --check` pass;
  the corrected fixture has not run in CI yet.
- **Component:** `nodemigrate` Cilium recovery / control-plane identity. The
  five-node lane imported 55 CRDs and stopped during the replacement Cilium
  readiness wait on `cp-1`. Diagnostics show `clean-cilium-state` exited 0 on
  all five nodes, while Cilium and kubelet clients reported TLS
  `unknown authority` against the API at `cp-1:6443` in diagnostics collected
  after rollback. The leading cause is a source/destination API trust-identity
  handoff problem, not a failed Cilium cleanup init or a sandbox-start failure.
  The branch now copies the source serving CA/key into the protected export and
  uses it to sign the replacement API certificate; for K3s it also preserves
  the distinct source client CA in a combined client-auth bundle. Joining an
  existing not-k8s cluster still uses that destination's PKI. Focused CI and
  the five-node migration regression are pending; no migration retry has been
  launched.

The migration control-plane teardown policy is recorded in
`NODEMIGRATION_GOAL.md`: save API/data/configuration and a restoration path
first; remove conflicting source services or old member state when required;
preserve source quorum and rollback material until destination quorum and
API/data are verified. This allows a full migration to retire old control-plane
parts without making destructive teardown the first step.

## Current blockers found in migration run 36540040356

- **Component:** `nodeapiserver` CRD status-subresource GET. Both single-node
  migration lanes imported the versioned `MigrationRecord` CRD and its object,
  then received HTTP 404 for `GET .../migration-record-0/status`. The listener
  implemented GET only for certificate status subresources, although generic
  CRD status writes were already supported. Added generic status GET through
  REST resolution, gated on the requested CRD version declaring the status
  subresource. Extended the API e2e regression and the migration checkpoint
  to GET `/status`; the `nodeapiserver,nodelet` scoped quick-check passed in
  [run 36545378947](https://github.com/centerionware/not-k8s/actions/runs/36545378947)
  at SHA `2cb26b82e8f9fc9efb6bc30dd647ad82616e61d7`.
- **Component:** `nodelet` CRI cgroup layout. The five-node migration imported
  535 API objects and accepted all 55 CRDs. Cilium's cleanup init exited 0 on
  `cp-1`, but its replacement agent container could not start. containerd's
  runtime config reported `SystemdCgroup=true`; nodelet sent cgroupfs paths
  such as `/kubepods/burstable/pod<uid>`, and runc rejected them because it
  expected systemd slice syntax. Nodelet now queries CRI `RuntimeConfig` and
  builds QoS-scoped cgroupfs or systemd parents accordingly, with focused
  path regressions. The nodelet CRI-feature quick-check passed at SHA
  `2cb26b82e8f9fc9efb6bc30dd647ad82616e61d7`. The TLS unknown-authority
  errors in the same artifact were emitted during post-rollback diagnostics,
  not as the migration's initiating failure.

Migration run [36540040356](https://github.com/centerionware/not-k8s/actions/runs/36540040356)
used branch runtime at `4cebc30721f96c034f48653345fd5b2f22e81ca7`. No migration
retry is considered until both fixes pass their scoped CI checks.

- **Scoped build correction:** Quick-check
  [36544899369](https://github.com/centerionware/not-k8s/actions/runs/36544899369)
  confirmed the new cgroup logic but failed to compile because CRI's
  `CgroupDriver` enum is top-level in the generated module, not nested under
  `LinuxRuntimeConfiguration`. The code now imports the generated top-level
  enum. The independent nodemigrate check
  [36544899266](https://github.com/centerionware/not-k8s/actions/runs/36544899266)
  passed packaging validation and all crate tests. Corrected scoped checks are
  pending, then passed both affected crates in
  [run 36545378947](https://github.com/centerionware/not-k8s/actions/runs/36545378947)
  after the correction at `2cb26b82`. No build.yml or full e2e gate was run.

## Custom-resource status fixture CRD placement

- **Component:** Migration integration fixture. Diagnostic run
  [36525843073](https://github.com/centerionware/not-k8s/actions/runs/36525843073)
  built the branch runtime and reached source fixture setup, but Kubernetes
  rejected the `MigrationRecord` CRD because `spec.subresources` was placed at
  the CRD spec level. In apiextensions/v1, each version owns its subresource
  declaration. The fixture now declares `subresources.status` under each
  served version, and a focused source check rejects a top-level placement or a
  missing per-version declaration. Shell syntax, fixture-structure,
  diagnostics, API-inventory, and whitespace checks pass locally. The source
  fixture and Cilium cutover diagnostic still need a live rerun; the failed run
  did not reach the Cilium handoff and provides no new datapath evidence.

## Standalone Pod ephemeral-container import

- **Component:** `nodemigrate` API transfer and integration fixture.
  Import previously passed `spec.ephemeralContainers` through ordinary Pod
  create/update calls. Kubernetes accepts those entries only on the Pod's
  `ephemeralcontainers` subresource, so a standalone source Pod with a debug
  container could abort the entire object import. The importer now strips the
  field from normal writes, then restores the source list with a strategic
  merge patch after the Pod exists. Strategic merge keys by container name,
  making a retried import append the same debug entries without duplication.
  The migration fixture now exercises adding and running an ephemeral
  container at every stage. Nodemigrate crate tests and the migration-script
  validation passed at SHA `62c4e1c8a2f3e83f56ebd7ffde11abf61d616fbf` in
  [nodemigrate checks run 36525276094](https://github.com/centerionware/not-k8s/actions/runs/36525276094)
  and [migration validation run 36525276074](https://github.com/centerionware/not-k8s/actions/runs/36525276074).
  The first focused crate run, [36525076582](https://github.com/centerionware/not-k8s/actions/runs/36525076582),
  caught use of the typed Pod helper on a dynamically discovered API; the
  importer now uses the generic dynamic subresource request. No live round trip
  has verified this path.

## Current source-cutover correction (focused quick-check passed)

- **Forward cutover bulk-removed ordinary CRI sandboxes before migration.**
  This destroyed runtime records that should remain available for recovery and
  made the workflow depend on per-Pod teardown. The source path now exports
  Kubernetes API objects while the API is live, disables the full source
  service/runtime stack, removes only source Cilium sandboxes and kubeadm
  control-plane API/etcd sandboxes when needed, then snapshots local PV data
  with writers stopped. Ordinary sandboxes and runtime data remain on disk.
  Failed snapshots/imports stop the partial destination, restore saved local
  PV/CNI payloads, and restore the prior runtime/service state. Nodemigrate-only
  quick-check [36522042632](https://github.com/centerionware/not-k8s/actions/runs/36522042632)
  passed at SHA `6c55fba88c47a7e1f0a5e5ef565cdec9cdfed825`; no migration run has
  exercised this change.
- **Late migration errors could leave the destination running and source
  disabled.** Forward control-plane/worker checks after destination startup and
  reverse worker readiness/scheduling failures now route through rollback to
  stop the partial target, remove only identified Cilium sandboxes/processes,
  restore local payloads, and restart the source. Ordinary Pod sandboxes remain
  untouched. The earlier quick-check
  [36521140201](https://github.com/centerionware/not-k8s/actions/runs/36521140201)
  found a unit-test type mismatch in the targeted Cilium sandbox selector;
  the fix and rollback changes passed nodemigrate-only quick-check
  [36522042632](https://github.com/centerionware/not-k8s/actions/runs/36522042632).
  No migration retry has been dispatched.

## Latest branch-run findings

- **Cilium restart diagnostic did not force agent-container recreation.** In
  [run 36526954453](https://github.com/centerionware/not-k8s/actions/runs/36526954453),
  the source fixture, Cilium Pod recreation, retained API-state checks, and
  `10.43.0.1:443` Pod-to-Service probe all passed. The follow-up restart probe
  stopped and removed the Cilium CRI pod sandboxes, restarted K3s, and confirmed
  the Node UID was unchanged, but the Cilium agent container ID had not changed.
  This showed the probe was not forcing the same full Cilium teardown that the
  migration path requires; it did not reproduce or clear the post-return
  Cilium datapath failure. The probe now stops/removes only running Cilium
  containers before removing their sandboxes, then requires a new ready agent
  container identity. The correction passed at SHA
  `76e2e3712c1e6b39781b305f00261307fabe6403` in
  [run 36528530369](https://github.com/centerionware/not-k8s/actions/runs/36528530369):
  Cilium's container ID changed; the API ClusterIP probe, complete source
  fixture checks, unchanged-UID restart, and same-name Node replacement all
  passed. This validates a single-node K3s+Cilium source teardown/restart, not
  cross-cluster migration; the post-return datapath issue remains unresolved.
  The saved logs are `/tmp/nodemigrate-36526954453/` and
  `/tmp/nodemigrate-36528530369/`.

- **K3s return leaves ClusterIP/pod routing broken after Cilium reports ready.**
  In K3s job `109195297576` of run
  [36502166212](https://github.com/centerionware/not-k8s/actions/runs/36502166212),
  the return migration passed Node replacement and audit checks. Returned-stage
  verification then failed because `metrics-server` repeatedly panicked when
  its in-cluster request for `kube-system/extension-apiserver-authentication`
  reached `10.43.0.1:443` with `no route to host`; pod-IP probes also reported
  no route during recovery. Cilium later reported KubeProxyReplacement true
  and all controllers healthy, but no service/BPF LB/endpoint map dump was
  collected. This is a real post-return Cilium datapath failure with an
  unconfirmed mechanism. Do not count the run as passed, hide the failed
  workload check, or dispatch another migration until the cause is fixed.
  Additional captured agent logs repeatedly report endpoint devices such as
  `lxc0bd1f34c7d60` missing and say a `CiliumEndpoint` was deleted externally
  while endpoint synchronization would recreate it. The pod IP and endpoint
  identity still appear in agent state. These stale-endpoint messages are a
  lead, not proof that they caused the Service routing failure. The saved Pod
  status adds a second concrete lead: the source `cilium` Pod kept the same UID
  while its agent container restarted after nodestore had used the host. Its
  `clean-cilium-state` init container completed only during the original Pod
  initialization; that init does not rerun for a regular container restart.
  Cilium documents that bpffs pins BPF resources across agent restarts, so
  source/destination host-state reuse is plausible ([Cilium bpffs
  persistence](https://docs.cilium.io/en/stable/operations/system_requirements/)),
  but the failed run lacks the returned-stage service/BPF map dump needed to
  prove it. A Pod recreation by itself is not yet a fix: Cilium's
  [`cleanState` Helm
  value](https://github.com/cilium/cilium/blob/v1.20.2/install/kubernetes/cilium/values.yaml)
  defaults to false, and the init container's presence alone does not show
  that it erased state.
  Any cleanup fix must reset only this node's datapath, preserve/reinstall its
  CNI configuration and binaries, and verify the returned Service backend and
  Pod-to-API route before another full migration attempt.
  Failure diagnostics capture `cilium-dbg service list`, `bpf lb list`, and
  `endpoint list`; they now also record the Cilium Pod UID, cleanup-init exit
  code, and `clean-cilium-state`/`clean-cilium-bpf-state` settings so the next
  failure can distinguish persisted host state from API reconciliation.
  A diagnostic probe [36506647392](https://github.com/centerionware/not-k8s/actions/runs/36506647392)
  passed a K3s service restart with the original Node UID and repeated all
  workload/API-state checks successfully. This shows an ordinary K3s restart
  alone does not reproduce the outage; it does not establish the cause of the
  migration failure. The diagnostic run
  [36509703232](https://github.com/centerionware/not-k8s/actions/runs/36509703232)
  completed same-name Node deletion/re-registration with all fixture health,
  storage, and 119 API-resource checks passing. Cilium reported 83/83 healthy
  controllers and an active BPF backend for `10.43.0.1:443` after replacement.
  The only parity difference was K3s rotating the generated
  `kube-system/<node>.node-password.k3s` Secret's `/data/hash`; the diagnostic
  comparator now permits only that exact generated credential mutation during
  replacement. The corrected live probe
  [36511406969](https://github.com/centerionware/not-k8s/actions/runs/36511406969)
  passed all stages at SHA `11a401e50ddcc1d0f83d5ecc0970a9b4545bbb89`.
  Cilium remained at 83/83 healthy controllers with an active API ClusterIP
  BPF backend before restart, after restart, and after replacement. This
  disproves bare restart/replacement as the cause, but not the cross-cluster
  migration path. That failure remains unresolved; no migration retry until
  its cause is fixed. Map presence alone does not prove Pod packet delivery;
  run [36513067748](https://github.com/centerionware/not-k8s/actions/runs/36513067748)
  passed an in-Pod TCP probe to the API ClusterIP before and after restart and
  Node replacement. The Cilium agent Pod UID did not change in those stages.
  Since migration recreates that Pod, run
  [36514477496](https://github.com/centerionware/not-k8s/actions/runs/36514477496)
  attempted the explicit restart but failed first: its jq selector returned a
  boolean rather than the Pod object. The selector is fixed and a focused
  check covers old-UID to new-UID Ready Pod replacement. Corrected diagnostic
  [36515656678](https://github.com/centerionware/not-k8s/actions/runs/36515656678)
  passed at SHA `b3fbd01f5d5bcf438727a7aecd3a515eff9f7f6a`: the new Cilium Pod
  became Ready, an in-Pod TCP probe to API ClusterIP `10.43.0.1:443` passed,
  and workload, storage, API inventory, and Node replacement assertions
  passed. Agent Pod recreation alone does not reproduce the issue. The
  migration-specific Cilium cause remains unresolved and migration must not be
  retried. The next diagnostic exercises the complete CRI sandbox stop/remove
  sequence with Cilium last, then restarts K3s and repeats API ClusterIP and
  fixture checks before Node replacement. Run
  [36518048359](https://github.com/centerionware/not-k8s/actions/runs/36518048359)
  stopped before K3s restart when a stopped sandbox removal returned a
  canceled CRI RPC. The probe aborted, while production only warns after
  confirming no containers remain running. The diagnostic now applies the
  same recovery rule; a local stub injects that remove failure and verifies
  all later Cilium-last removal calls continue. Corrected live evidence is
  pending. It does not run nodemigrate.
- **Replacement diagnostic used a kubectl flag that is not supported.** The
  unchanged-UID restart and all fixture checks passed in run
  [36508298004](https://github.com/centerionware/not-k8s/actions/runs/36508298004),
  then the probe stopped at `kubectl delete --preconditions`. The diagnostic
  now checks the current Node UID immediately before deletion, uses supported
  kubectl delete flags, and verifies the Node is absent before restarting
  K3s. Since Node replacement never ran in that attempt, the Cilium issue is
  still unresolved; do not rerun migration until its actual cause is fixed.
- **Replacement parity comparison treated a K3s-generated Node credential as
  durable user state.** After same-name Node deletion/re-registration, run
  [36509703232](https://github.com/centerionware/not-k8s/actions/runs/36509703232)
  passed fixture, storage, API-resource, and Cilium service-map checks; strict
  normalized parity found only the regenerated node-password Secret hash.
  The comparator now allows only `/data/hash` for the exact K3s-managed Secret
  and only in the replacement diagnostic. This test-harness correction does
  not waive parity for user objects or migration stages. Error propagation is
  explicit, with focused passing cases for expected hash rotation, unexpected
  changes to that Secret, and changed user fixture data. Live diagnostic
  recheck pending.
- **Five-node fixture used unsupported container-to-container `docker cp`.**
  Docker job `109195297329` in run `36502166212` passed topology, Cilium, and
  control-plane recovery checks, then failed while copying PKI/kubeconfig
  files. Docker requires container-to-host or host-to-container copies. The
  helper now stages each file through a host temporary file and streams it to
  the destination container. Syntax and focused diagnostics checks pass; the
  repaired copy path still needs the next permitted migration batch for live
  verification.

- **Same-name Node deletion is asynchronous during return cutover.** K3s job
  `109181807925` in run
  [36497949349](https://github.com/centerionware/not-k8s/actions/runs/36497949349)
  accepted a UID-preconditioned Node DELETE, but the still-running K3s agent
  raced registration while that UID was terminating. The cutover then tested
  readiness before the old UID had been removed and timed out. `delete_node`
  now waits, with a 60-second bound, until the old Node is absent or a new UID
  has registered before cutover continues. Focused tests cover the old, absent,
  and new-UID states; CI validation and a live return leg remain pending.
- **CSINode owner-reference repair omitted required spec on its patch.** The
  five-node Docker job `109181808357` in run `36497949349` passed kubeadm,
  Cilium, and control-plane recovery preflight, then the forward migration was
  rejected while repairing `CSINode/cp-1` because `spec.drivers` was required.
  Initial import already normalizes this field, but the later owner-reference
  merge patch only sent metadata. The repair now carries forward the driver's
  list or supplies an empty list if absent, with regressions for both cases.
  Focused CI and a new five-node migration run remain pending.
- **Deployment controller did not scale its only ReplicaSet down.** In the
  K3s target audit in run `36497949349`, the fixture scaled nginx from two
  replicas to one; its Deployment remained `spec.replicas=1` with two Ready
  Pods and PDB `currentHealthy=2`, `desiredHealthy=1`, and
  `disruptionsAllowed=1`. `new_rs_desired_replicas` held the current count at
  the surge ceiling instead of reducing it to the new desired count. It now
  explicitly scales an over-sized current ReplicaSet back to `desired` and
  has a focused regression. CI validation is pending. The migration PDB
  assertion remains unchanged.

- **Replicated core/v1 ReplicationController default differs in snapshots.** Run
  [36490156839](https://github.com/centerionware/not-k8s/actions/runs/36490156839)
  timed out waiting for `migration-replication-controller` to report one Ready
  replica in both the K3s-to-nodestore and Kubernetes-to-nodestore lanes. The
  fixture had first passed on each upstream source. nodecontroller registered
  no ReplicationController reconciler. The working branch now adds one using
  the existing watch-driven ReplicaSet pattern, registers the shared RC watch,
  binds the upstream `replication-controller` identity, and grants its status
  patch. The migration script prints RC, Pod, and event state on a timeout.
  Follow-up run [36493677637](https://github.com/centerionware/not-k8s/actions/runs/36493677637)
  confirms the new reconciler creates and readies the owned Pod in both lanes.
  Strict normalized parity then found only `/spec/minReadySeconds`: upstream
  omitted default zero while not-k8s returned explicit zero. The working branch
  now treats omitted and zero as equivalent in both snapshot filters and keeps
  nonzero values strict. CI parity validation remains pending.
- **CSINode import fails when `spec.drivers` is null.** Five-node job
  `109156567171` in run `36490156839` failed applying source CSINodes with
  `spec.drivers: Required value`; run `36493677637` failed the same way. The
  importer already materialized an omitted `spec`, but retained a present
  `drivers: null`. The working branch now replaces every non-array driver value
  with an empty list and has regressions for absent `spec` and null `drivers`.
  The five-node import remains unverified after this correction.
- **Migration target certificate omits the source API endpoint SAN.** In run
  `36493677637`, the kubeadm source certificate correctly included `cp-1`,
  `cp-2`, and `cp-3`, but node kubelets later rejected the replacement
  not-k8s API certificate for `cp-1`. The target PKI was generated from the
  node's IP address and did not preserve the source kubeconfig's DNS endpoint.
  The working branch extracts the current source API host and supplies it as
  an additional not-k8s API serving SAN. The existing DNS endpoint and
  migration-time certificate behavior need CI verification.

- **K3s returned Node disappears after becoming Ready.** In K3s job
  `109084193351` of run
  [36468405459](https://github.com/centerionware/not-k8s/actions/runs/36468405459),
  the watcher saw the returned Node at 19:11:04, 19:11:12, and 19:11:19 UTC,
  then saw an empty Node list from 19:11:33 onward. K3s subsequently reported
  the Node missing and could not satisfy PV NodeAffinity or register the
  HostPath CSI driver. The captured audit stream ends at 19:10:04, before the
  return-migration service restart and the Node disappearance, so it cannot
  establish whether a DELETE occurred or identify the actor. The actor and
  mechanism remain unknown; do not paper over this by relaxing readiness or
  storage checks. The fixture configures K3s API auditing for Node and
  `kube-node-lease` Lease mutations and prints that audit file on failure. It
  now also verifies immediately after return migration that the active K3s
  unit retains both audit arguments and the audit stream contains Node and
  Lease mutations newer than the return start time. A missing stream fails
  before CSI fixture reinstallation. Failure collection includes K3s unit
  state and K3s/kernel journals since migration start. The focused diagnostics
  check covers valid Node/Lease events and rejects a stream missing Node
  mutations; live audit and failure-actor evidence remain pending. Batch run
  [36490156839](https://github.com/centerionware/not-k8s/actions/runs/36490156839)
  validates the accumulated fixes with this audit gate enabled. The Node-loss
  issue remains open until its returned-stage evidence is reviewed.
  The watcher now records Node UID and lifecycle timestamps in its change
  detector, and captures Node resource versions plus Node-Lease renewal times
  at its normal diagnostic cadence. This preserves identity/heartbeat evidence
  without making every Lease renewal trigger a full log capture; only shell
  syntax and the focused `check_nodemigrate_diagnostics.sh` script pass locally.
  The check covers mocked JSON/audit diagnostics, not the live watcher or API
  audit path.
  The captured event list also contains K3s `RemovingNode` events and repeated
  `Starting kubelet` events. Upstream NodeLifecycleController emits
  `RemovingNode` after its informer list no longer contains a Node that was in
  its prior known-node set; the event reports an observed disappearance and
  does not mean that controller issued the delete. It does not establish why
  the Node stopped renewing or whether a direct DELETE request preceded it.
  The return path now requires a new Ready
  UID after it replaces an existing Node, preventing the old Ready object from
  satisfying cutover. Focused check
  [36481686171](https://github.com/centerionware/not-k8s/actions/runs/36481686171)
  passed at SHA `eb650b223a0503e135d4ff50b317d159b39bb2b4`. This guard has not
  been exercised in a migration run and does not close the later Node-loss
  failure. The failure artifact records the returned Node Ready at 19:11:04,
  19:11:12, and 19:11:19 UTC with Cilium reporting
  `NetworkUnavailable=False`; the Node list was empty by 19:11:33. Afterward,
  K3s kubelet logs show repeated `Forbidden` errors for its old Pod UIDs because
  the node authorizer could no longer find a Node-to-Pod relationship. These
  are downstream symptoms of the missing Node, not evidence that kubelet
  caused its deletion. The K3s audit data still ends before this interval, so
  the deleting actor and root cause remain unproven. Do not make a runtime
  behavior change based only on these downstream errors. The same post-return
  Node loss is present in saved K3s artifacts for runs
  [36434878771](https://github.com/centerionware/not-k8s/actions/runs/36434878771),
  [36459506591](https://github.com/centerionware/not-k8s/actions/runs/36459506591),
  [36463585411](https://github.com/centerionware/not-k8s/actions/runs/36463585411),
  and `36468405459`; the repeated failure confirms this is not an isolated
  HostPath CSI rollout issue. The configured Node-mutation audit must identify
  whether the API object is deleted or the K3s control plane loses it before
  changing Node lifecycle behavior.
  Upstream Kubernetes v1.35 source confirms `registerWithAPIServer()` returns
  immediately after the kubelet's first successful registration, while
  `syncNodeStatus()` continues to update the existing Node. A running kubelet
  therefore does not recreate a Node that disappears later. Its Lease
  controller has a separate missing-Lease recovery path, which does not
  establish a Node-loss cause. This confirms the post-loss `node not found`
  errors are expected aftermath and that the explicit service restart after
  the initial replacement delete is intentional. The later disappearance is
  still unexplained. Sources: [Kubernetes v1.35 kubelet node status](https://github.com/kubernetes/kubernetes/blob/v1.35.0/pkg/kubelet/kubelet_node_status.go)
  and [Node Lease controller](https://github.com/kubernetes/kubernetes/blob/v1.35.0/staging/src/k8s.io/component-helpers/apimachinery/lease/controller.go).
  The `RemovingNode` event is generated while classifying informer-listed
  Nodes, so it records the controller's observation rather than the API
  deletion actor: [NodeLifecycleController](https://github.com/kubernetes/kubernetes/blob/v1.35.0/pkg/controller/nodelifecycle/node_lifecycle_controller.go).
- **Empty CronJob status triggers repeated no-op writes.** The same K3s log
  records `system:serviceaccount:kube-system:cronjob-controller` patching the
  unchanged `migration-cron` status every few milliseconds. When a missing
  status and a default empty desired status compare unequal, the API can prune
  the empty patch and the next reconcile repeats it. `nodecontroller` now
  treats missing and default-empty status as equivalent and has a regression
  for the no-op case. Both this regression and the CSINode importer regression
  passed focused check
  [36473759900](https://github.com/centerionware/not-k8s/actions/runs/36473759900)
  at SHA `32255054dcc65baeac828f2ec00acd1b54ba3208`.
- **CSINode imports omit the required empty driver list.** Docker preflight
  job `109084192913` in run `36468405459` received four 422 errors because the
  source CSINode objects lacked `spec.drivers`. The importer now materializes
  an empty list only when the source omits the field, preserving the source's
  empty-driver meaning while satisfying the Kubernetes API schema. The focused
  regression passed in run `36473759900`; full migration verification remains
  pending.
- **Round-trip import changes Traefik CRD numeric bounds.** Upstream
  job `109084193399` in run `36468405459` found Traefik `maximum` bounds
  serialized once as integer and once as floating-point numbers. Checking the
  adjacent IEEE-754 values showed they are different `double` values, so JSON
  parity must remain strict. A proposed normalization was reverted. Focused
  `DynamicObject` tests passed in
  [36474893777](https://github.com/centerionware/not-k8s/actions/runs/36474893777),
  but a listener regression then showed that JSON-decoding a persisted HTTP
  read changed the value by one ULP. Enabling `serde_json/float_roundtrip`
  across the workspace fixes this parser boundary. Codec and listener
  regressions passed focused CI
  [36479715060](https://github.com/centerionware/not-k8s/actions/runs/36479715060)
  for `nodeapiserver,nodemigrate` at SHA
  `e253e25cf003ec04f8a59bb9e67988e748732135`. Runtime migration parity
  remains unverified; keep it strict and verify it after the K3s Node-loss
  failure is resolved.

- **Round-trip import changes Traefik CRD numeric bounds.** Upstream job
  `109067982540` in run
  [36463585411](https://github.com/centerionware/not-k8s/actions/runs/36463585411)
  passed the returned functional checks, then the strict object comparator
  found the `maximum` value changed for `ingressroutes.traefik.io` and
  `ingressroutetcps.traefik.io` from `9223372036854775000` to
  `9223372036854776000`. Nodestore returned this value as a floating-point
  JSON number (`9.223372036854776e+18`) while the retained Kubernetes API
  returned the earlier integer form. This is a genuine difference between
  adjacent IEEE-754 `double` values, not merely a JSON rendering change.
  Migration parity must stay strict while the export/import path is traced and
  fixed.
- **K3s removes the returned Node during HostPath CSI setup.** The same run's
  K3s lane passed source and nodestore checks, returned the Node as Ready, and
  then failed while reinstalling HostPath CSI. K3s emitted `Removing Node`
  events; StatefulSet PV node-affinity checks could not find the node, and
  HostPath CSI registration remained unavailable. This is after the utility's
  return command reported success, so the loss occurs during or after test
  fixture reinstallation. Determine which process deletes the Node and retain
  its identity and scheduling state through the full returned-stage check.
- **Docker five-node fixture lacked Python for source checkpoints.** In the
  same run, all five kubeadm nodes reached Ready with Cilium and the test passed
  its control-plane-loss and recovery checks. It then exited 127 at
  `nodemigrate-integration.sh:2826` because `python3` was absent from the
  Ubuntu node image. The five-node image now installs `python3`; rerun the full
  simulated path before closing this bug.

- **Returned PV identity repair creates an unsafe claim-reference window.** In
  upstream job `109054224944` of run
  [36459506591](https://github.com/centerionware/not-k8s/actions/runs/36459506591),
  every fixture PVC emitted `ClaimMisbound` after return import. The retained
  StatefulSet PV references the source PVC UID, but the importer removes
  `spec.claimRef.uid` for the initial PV write and repairs it in a later pass.
  The HostPath CSI driver subsequently reported that the original volume ID
  was missing from its catalog. This identifies `nodemigrate`'s PV import
  ordering/reference rewrite as the path to fix; the exact causal link from the
  transient ref removal to CSI catalog loss still requires runtime proof. The
  importer now orders PVCs before PVs and maps a known PVC UID before the first
  PV write. Unit regressions assert PVC-before-PV ordering, remapping a known
  claim UID before the initial PV write, and clearing an unmapped UID for
  later repair. They passed `cargo test -p nodemigrate` in
  [run 36486806547](https://github.com/centerionware/not-k8s/actions/runs/36486806547)
  at SHA `899143621b5aaf944a3ce0ab50ef97f4f547bd92`. The cross-component CSI
  data handoff and the causal link to the missing volume catalog still require
  migration runtime verification.
- **Docker's Cilium assertion inspected Helm overrides, not effective values.**
  Run `36459506591` logged `ipam.mode=kubernetes`, KPR `true`, and
  `cni.binPath=/opt/cni/bin`, while `cni.confPath` was omitted from stored
  overrides because the fixture uses the chart default. The fixture now checks
  `helm get values --all`; a rerun must confirm the merged default path and
  reach the migration checks.
- **K3s return cannot start HostPath CSI within five minutes.** The same run's
  K3s job reports a successful return, but HostPath CSI never becomes Ready.
  Nodelet logged Pod reconciliation timeouts for CoreDNS and the CSI plugin,
  and API logs contain Cilium peer certificate failures. This points toward
  nodelet/Cilium recovery after the service handoff, but does not yet isolate
  the failing transition. Later K3s captures show the Node disappears before
  HostPath CSI scheduling fails, so treat CSI registration and PV NodeAffinity
  failures as consequences of the confirmed Node-loss symptom until evidence
  separates them. Do not relax the readiness assertion.

- **Migration run 36434878771: returned HostPath CSI and Docker Cilium state
  checks fail.** At SHA `4f844b4311284e93fa6c8883c7858ae87122ee8a`, all three
  lanes compiled nodemigrate and combined branch `notk8s --features cri`; this
  compiles all changed components packaged in that runtime. Docker also built
  the five-node image. The Docker fixture passed source workload, static PV,
  CSI PV, network, and Traefik checks, then its Cilium values predicate failed
  without printing the observed values. The fixture now prints the IPAM,
  kube-proxy replacement, and CNI path values when the predicate fails. The
  check now reads Helm's effective merged values instead of requiring every
  expected field to have been explicitly overridden. This remains assigned to
  migration CI until a rerun confirms the chart's effective settings. K3s passed source and
  nodestore checkpoints and logged successful return migration, then the
  HostPath CSI deployment did not become Ready within five minutes. Nodelet
  logs show repeated teardown of CSI Pods during this stage, making
  `nodelet` Pod reconciliation the lead component to inspect; the triggering
  state transition is not yet isolated. Upstream passed through returned-stage
  parity, but restored workloads could not use HostPath CSI: the driver was
  reported unregistered, and a later mount reported that the retained volume
  ID was absent from the driver's volume catalog. This is tracked against
  `nodemigrate` CSI state handoff plus `nodelet` plugin registration until the
  logs identify the first broken handoff. No lane completed a full round trip
  with parity. Logs were downloaded once under
  `/tmp/nodemigrate-artifacts-36434878771/`; jobs K3s `108970087767`, upstream
  `108970088064`, Docker `108970088143`.

- **Latest K3s, upstream, and five-node migration run failed after all builds
  passed.** Branch-runtime run
  [36430036471](https://github.com/centerionware/not-k8s/actions/runs/36430036471)
  compiled nodemigrate and the combined `notk8s --features cri` runtime in all
  lanes, and built the five-node image. The Docker static/CSI data probes both
  passed, but the source verifier exited after the Traefik rollout before
  identifying its failing assertion. The Helm assertions now print a specific
  failure message. K3s reported the returned Node Ready, then its watcher
  recorded an empty Node list by 13:59:32 UTC; no evidence identifies who
  removed it. Upstream reached `PASS stage=returned`, but strict parity still
  found both Traefik CRD `priority.maximum` fields changed from
  `9223372036854775000` to `9223372036854776000`. The metadata-only patch was
  not used for those CRDs. A focused comparison diagnostic now logs the
  existing and desired maximums. Artifacts were downloaded once to
  `/tmp/nodemigrate-artifacts-36430036471/`.

- **Five-node source-stage failure was previously silent.** The failure log
  stopped after `deployment traefik successfully rolled out`; later Helm and
  Cilium value checks used `set -e` without naming their failing condition.
  Those checks now emit a specific error before returning. The next run will
  identify the exact condition.

- **Docker static-data checker could not schedule on its PV node.** Migration
  run [36425151654](https://github.com/centerionware/not-k8s/actions/runs/36425151654)
  built nodemigrate, the combined branch runtime, and the five-node image, then
  timed out waiting for `migration-static-data-check`. The PV is pinned to
  cp-1, whose control-plane taint the checker did not tolerate. A matching
  `NoSchedule` toleration is now in the fixture; migration verification is
  pending.

- **K3s Node disappeared after return migration reported success.** Run
  `36425151654` reached `Migration to K3s completed`, then later reported the
  returned Node missing while checking the local PV and workloads. Available
  evidence does not identify the actor or establish why it disappeared. Keep
  this unresolved; the return-target state watcher now remains active through
  returned-stage checks to capture the Node transition before any runtime
  behavior is changed on an assumed cause.

- **Return import changed Traefik CRD numeric schema bounds.** In upstream run
  `36425151654`, source and nodestore retained `9223372036854775000`, while
  strict returned-state parity observed `9223372036854776000`. Exact-object
  preservation was insufficient because metadata differed. The importer now
  compares CRD specs and, when equal, applies a metadata-only merge patch so
  the schema is not round-tripped through a full-object write. The patch and
  regression are pending focused CI and migration verification. Strict parity
  remains enabled.

- **Previous migration utility test passed.** Focused check
  [36425866401](https://github.com/centerionware/not-k8s/actions/runs/36425866401)
  passed at SHA `8b11b11f3f26546f0a08bdcaba99382bb2fdcb6b`, including
  nodemigrate crate tests. It predates the CRD metadata-patch regression added
  afterward and does not verify that change.

- **CRD metadata-patch test needed a qualified JSON null value.** Focused run
  [36429944327](https://github.com/centerionware/not-k8s/actions/runs/36429944327)
  at SHA `de520706339fce833e6ff1099d760ad8f1ca71ee` compiled the library, then
  failed compiling the new unit test because `Value::Null` was not imported in
  the test module. The assertion now uses `serde_json::Value::Null`; a focused
  rerun passed in
  [36430618866](https://github.com/centerionware/not-k8s/actions/runs/36430618866)
  at SHA `37da8cde9d1bc2489bfe588d379499b55b929e39`.

## Latest branch-run findings

- **CRD preservation predicate did not compile.** Nodmigrate run
  [36424188417](https://github.com/centerionware/not-k8s/actions/runs/36424188417)
  and migration run
  [36424227253](https://github.com/centerionware/not-k8s/actions/runs/36424227253)
  at SHA `b140e73d7e57fdafdcb9105767b9e6056954abb6` failed because
  `DynamicObject.types` is `Option<TypeMeta>`. The predicate now checks the
  optional TypeMeta before reading `kind`; no tests or migration lanes ran past
  compilation on the failed SHA. Its first correction then failed in
  [run 36424544812](https://github.com/centerionware/not-k8s/actions/runs/36424544812)
  and migration dispatch
  [36424565854](https://github.com/centerionware/not-k8s/actions/runs/36424565854)
  because `TypeMeta.kind` is a `String`, not `Option<String>`. The predicate
  now guards the optional TypeMeta and compares its kind directly. CI
  compiled nodemigrate in both single-node migration lanes, and run
  [36425151654](https://github.com/centerionware/not-k8s/actions/runs/36425151654)
  is still in progress. The focused test run
  [36425123437](https://github.com/centerionware/not-k8s/actions/runs/36425123437)
  found that large JSON test literals need `u64` suffixes; the literals are
  fixed locally and test verification is pending.

## Latest branch-run findings

- **Kubectl client version probe used an unsupported output format.** In
  [run 36420315264](https://github.com/centerionware/not-k8s/actions/runs/36420315264),
  Docker passed the Cilium KPR, five-node recovery, and plugin-node CSI checks,
  then failed when the fixture requested `kubectl version --client -o
  jsonpath`. The helper now reads `gitVersion` from YAML output. This fixture
  correction awaits rerun.

- **Return import rewrote an unchanged CRD schema.** The same run's upstream
  lane showed the Traefik schema `maximum` unchanged at the source and
  nodestore checkpoints (`9223372036854775000`), then rounded at the return
  API. The importer previously sent a full-object update even when the
  destination CRD already held identical spec and relevant labels/annotations.
  It now skips that rewrite in this exact-match case; a focused unit test checks
  unchanged CRDs and verifies that schema or label differences still require
  the normal update path. Runtime verification is pending.

- **K3s return lost the Node after migration reported success.** In run
  `36420315264`, migration reported completion and the returned Node initially
  became Ready. Later diagnostics found it missing; kubelet then reported no
  node relationship for imported Pods, CSI teardown could not find its driver,
  and the local PV could not resolve its node affinity. The capture does not
  identify which actor initiated the Node removal. Keep this as unresolved;
  capture audit/Node lifecycle events around the completion-to-fixture boundary
  before attributing or changing migration behavior.

## Latest branch-run findings

- **Fixture checkpoint probe used an unsupported CRD short name.** In run
  [36418026366](https://github.com/centerionware/not-k8s/actions/runs/36418026366),
  both K3s and upstream reached forward import and most nodestore workload
  checks, then `kubectl get crd` failed because the destination API did not
  recognize the short name. Python consequently reported an empty-input JSON
  decode error. The diagnostic now uses
  `customresourcedefinitions.apiextensions.k8s.io`; no CRD precision conclusion
  can be drawn from this run.

- **Five-node fixture did not initialize its kubectl Job image.** The same
  run passed Cilium KPR recovery and CSI registration on the actual plugin
  Node, then hit an unset `NODEMIGRATE_KUBECTL_IMAGE` while creating fixture
  RBAC Jobs. The normal `install_tools` path had set it, but the Docker
  entrypoint calls the shared fixture directly. `install_workloads` now derives
  the image tag from `kubectl version --client`; runtime verification is
  pending.

## Latest branch-run findings

- **The five-node CSI assertion assumed a singleton plugin runs on every
  Node.** Run
  [36414625541](https://github.com/centerionware/not-k8s/actions/runs/36414625541)
  verified Cilium KPR and five-node recovery after control-plane loss, then
  failed before nodemigrate ran: the upstream hostpath CSI StatefulSet had one
  Ready Pod on `worker-2`, while the check required `hostpath.csi.k8s.io` in
  every Node's `CSINode`. The assertion now derives the expected Nodes from
  Ready plugin Pods and checks their driver registration and Node owner refs.
  Runtime verification is pending.

- **K3s return still loses Node/CSI readiness after reporting completion.** In
  run `36414625541`, source and nodestore checkpoints passed and nodemigrate
  reported migration to K3s complete. The later hostpath CSI deployment failed
  its five-minute readiness wait. Captured K3s logs then repeatedly report its
  own Node missing and Lease owner-ref reconciliation failing; kubelet reports
  no Node relationship for re-imported Pods. Hostpath logs also report missing
  CSI driver registration and volume IDs absent from its catalog. The logs do
  not identify which transition initiated the churn; preserve as unresolved
  and instrument the Node deletion/re-registration boundary before changing
  migration behavior.

- **Traefik CRD schema bound still changes on round trip.** Upstream run
  `36414625541` passed source, nodestore, and returned workload/storage
  checkpoints, then strict parity found `priority.maximum` changing from
  `9223372036854775000` to `9223372036854776000` in the HTTP and TCP IngressRoute
  CRDs. This is a real numeric value difference; parity remains strict and the
  responsible migration hop is still unisolated.

## Latest branch-run findings

- **Cilium bootstrap cannot depend on CoreDNS to resolve the API host.** In
  five-node Docker run
  [36411280751](https://github.com/centerionware/not-k8s/actions/runs/36411280751),
  the resolver-file change removed CoreDNS's loop, but Cilium still tried to
  resolve `cp-1` through the upstream resolver before CoreDNS could serve
  cluster DNS. The fixture now gives Cilium cp-1's reachable IP as
  `k8sServiceHost`; runtime verification is pending.

- **K3s return completed, but hostpath CSI did not converge afterward.** In
  run `36411280751`, nodemigrate reported the returned Node Ready and migration
  complete. The follow-up hostpath deployment then exceeded five minutes
  waiting for readiness while Cilium agent/Envoy health and Node/Pod state
  churned. Kubelet logged missing Node and re-imported Pod relationship
  messages, and CSI reported volume IDs absent from its volume list. These are
  observed symptoms; the available logs do not establish a single cause or
  which actor changed the Node/Pods. Preserve this as unresolved until a run
  captures the transition at the failure boundary.

- **Traefik CRD schema bound still changes on round trip.** Upstream run
  `36411280751` again found `priority.maximum` changing from
  `9223372036854775000` to `9223372036854776000` after return. This is a real
  numeric value difference and strict parity remains enabled. The current
  checkpoints need inspection to isolate whether export, nodestore transfer,
  or API storage changes it.

## Latest branch-run findings

- **CSINode registration trails replacement Node readiness.** In migration run
  [36397547121](https://github.com/centerionware/not-k8s/actions/runs/36397547121),
  K3s imported all CRDs and returned the Node to Ready, but the CSI registrar
  had not recreated that Node's `CSINode` yet. Owner-reference repair failed
  on the immediate 404. `nodemigrate` now waits up to 60 seconds for this
  specific node-scoped object before patching its source Node owner reference;
  another runtime run is pending.

- **The five-node seed Pod combined volumes pinned to different Nodes.** In
  run `36397547121`, the static hostPath PV correctly targeted `cp-1`, while
  the hostpath CSI PV was provisioned on `worker-2`. One Pod mounting both
  could not satisfy both PV affinities. The fixture now seeds and verifies
  each PVC in a separate Pod, preserving the static data on the migrating
  Node and exercising CSI data on the driver's Node; Docker verification is
  pending.

- **CRD schema `maximum` still changes across the branch API round trip.** Run
  `36397547121` passed the upstream return workload and storage checks but
  strict parity again found two Traefik CRD `priority.maximum` values change
  from `9223372036854775000` to `9223372036854776000`. Parity remains strict.
  The regression is now covered by a focused nodeapiserver CRD round-trip test
  so the protobuf encode/decode path can be checked directly.

- **The five-node probe's CoreDNS pods crash-loop independently of the seed
  PV scheduling error.** The `36397547121` diagnostic snapshot shows the
  kubeadm cluster's two CoreDNS Pods in `CrashLoopBackOff` while Cilium agents
  and the fixture workloads are running. This needs separate diagnosis after
  the storage fixture can schedule and the probe reports its next blocker.

- **`node_exists` discovery did not retry a transient K3s 503.** In
  [run 36393910249](https://github.com/centerionware/not-k8s/actions/runs/36393910249),
  K3s imported the protected API and restored the workloads, but the post-import
  replacement-node check made a direct discovery request while Cilium was
  recovering and received HTTP 503. The remaining `KubeApi` operations now use
  the bounded aggregate-first discovery retry helper; migration verification
  is pending.

- **Five-node static PV affinity used the wrong hostname value.** In run
  `36393910249`, the fixture tolerated cp-1's control-plane taint, but the
  scheduler reported that all five Nodes failed the static PV affinity. The
  fixture previously assumed the node name equaled its `kubernetes.io/hostname`
  label. It now reads the selected Node's actual label before creating the PV;
  Docker verification is pending.

- **Returned Traefik CRD schema bounds changed by one float64 step.** In run
  `36393910249`, source and nodestore checkpoints passed and the upstream
  return workload/storage checks passed, but two Traefik CRDs changed
  `priority.maximum` from `9223372036854775000` to `9223372036854776000`. These
  values are adjacent float64s, so parity remains strict and this is an
  unresolved migration data change. Trace the value through nodemigrate export,
  nodestore transfer, and source API write/read before considering any compare
  normalization.

- **ControllerRevision parity can ignore its generated sequence only.** The
  same upstream result showed `/revision` was the sole changed field on the
  existing CSI ControllerRevision; inventory now omits that controller-owned
  sequence but continues to compare the complete payload and other fields.
  The updated comparison passed the upstream inventory check before the CRD
  bound differences caused the run to fail.

- **Migration discovery must survive transient API 503s during CRD readiness.**
  In run [36390001684](https://github.com/centerionware/not-k8s/actions/runs/36390001684),
  K3s returned HTTP 503 from API discovery after accepting all 59 imported CRDs
  while Cilium components restarted. CRD readiness used a one-shot per-group
  discovery call and failed immediately. It now uses the aggregate-first
  compatibility path and retries transient discovery failures within the
  bounded CRD readiness deadline. The retry fix needs another K3s+Cilium
  migration run.

- **Five-node static hostPath fixture must schedule on its PV node.** The same
  run's source setup pinned `migration-static-pv` to `cp-1`, but the
  `migration-seed` Pod did not tolerate the control-plane `NoSchedule` taint and
  remained Pending for ten minutes. The fixture Pod now tolerates that taint;
  five-node preflight verification is pending.

- **CRD schema numeric bound changed during migration.** The
  upstream return in run `36390001684` reported Traefik `maximum` values as
  `9223372036854775000` and `9223372036854776000`. These map to adjacent
  float64 values, so this is a real schema change, not formatting. API
  inventory stays strict; trace the value through export, nodestore, and
  nodeapiserver CRD storage before changing comparison behavior.

- **CSI ControllerRevision sequence advances during harness reinstall.** Upstream return in
  run `36390001684` changed `/revision` on the existing
  `default/csi-hostpath-socat-574cd475f4` ControllerRevision. The harness
  reinstalls the fixture CSI components after cutover. The field-level
  inventory reported `/revision` as the only changed field for this object;
  comparison now excludes only that controller-assigned sequence while
  checking the original payload and other source fields strictly. The rule
  still needs runtime verification.

- **`nodemigrate`: aggregated discovery retry wrapped `anyhow::Error` twice.**
  At branch SHA `f8cc4777` both targeted crate testing and every migration job
  failed at compile time with E0277 in `wait_for_discovery`; no migration
  runtime started. The retry loop now adds context directly to the existing
  error. A pushed build/test rerun is pending.

- **Round-trip inventory needed to separate preserved source state from
  regenerated target state.** Run
  [36385002094](https://github.com/centerionware/not-k8s/actions/runs/36385002094)
  at SHA `9c7897f0` showed 526 source API objects preserved at nodestore and 82
  target-only records: nodebootstrap-owned RBAC, nodecontroller-generated
  EndpointSlices, and controller-owned ReplicaSets. The returned semantic
  summary then failed on those additions without explicitly proving every
  original object returned unchanged. The check now compares source objects
  against returned objects, requires each target-only object to have an exact
  runtime-generation classification, and reports CRD schema diffs. This keeps
  original ReplicaSet/ControllerRevision history and ordinary RBAC under the
  source-object parity check. Targeted script checks pass; migration CI is
  pending.

- **Do not migrate nodebootstrap's runtime-owned RBAC back into the retained
  source.** The same run found `nodebootstrap:` Roles, RoleBindings,
  ClusterRoles, and ClusterRoleBindings added by the destination runtime.
  Export now skips only those four RBAC kinds with the reserved prefix, because
  nodebootstrap provisions them for the active runtime. A focused regression
  checks that ordinary user RBAC and non-RBAC resources with similar names
  remain migratable. CI compilation and migration validation are pending.

- **K3s API discovery and five-node hostPath fixture fixes are pending
  runtime verification.** In run 36385002094 K3s passed direct retained API
  readiness but its full per-group discovery probes timed out while Cilium was
  unhealthy; the utility now tries aggregated discovery before fallback. The
  five-node fixture's static hostPath PV is now constrained to `cp-1`, where
  setup created its data directory. Neither follow-up has run in migration CI.

- **`nodemigrate`: stale generated EndpointSlices broke returned Traefik
  traffic.** In run
  [36381052266](https://github.com/centerionware/not-k8s/actions/runs/36381052266),
  upstream return import and Node registration completed; both Traefik route
  probes returned 504 with nginx and Traefik Pods Ready. The protected reverse
  export contained 11 EndpointSlices carrying this project's
  `endpointslice.kubernetes.io/managed-by=nodecontroller` label. The exporter
  skipped Kubernetes' two standard manager labels but not `nodecontroller`,
  so destination-rebuilt slices were replaced by stale pod IPs. nodemigrate
  now classifies `nodecontroller` slices as controller-managed and has a
  regression for that label. Quick-check and migration verification are
  pending.

- **K3s fixture raced Cilium sandbox replacement at the `emptyDir` check.**
  Run `36381052266` passed the Cilium and CoreDNS rollouts, then `kubectl exec`
  hit `CONTAINER_EXITED` for the selected API-Ready probe Pod during Cilium
  sandbox churn. Diagnostics show the replacement Pod/container being created
  shortly after the probe failed. The fixture now requires a Ready Pod with a
  running container ID and retries only transient runtime-exec failures while
  it re-reads Pod state; persistent failures still fail with Pod diagnostics.
  Verification is pending.

- **Five-node preflight image lacked tools required by its fixture.** The same
  run passed five-node isolation, kubeadm+Cilium setup, cp-1 loss/recovery, and
  Cilium rollout after the shared-mount fix, then the source hostpath installer
  failed at `git clone` because `git` was absent from the node image. The image
  now includes `git` and `cri-tools` for fixture setup and CRI diagnostics.
  Docker preflight verification is pending.

- **K3s fixture chose a Running Pod with no running container.** In branch
  run [36377385003](https://github.com/centerionware/not-k8s/actions/runs/36377385003),
  forward migration and 119-resource parity passed, but return rollback left
  the old non-root `emptyDir` Pod in phase Running with its container exited.
  The fixture now selects the newest non-terminating Pod only when the Pod and
  all containers are Ready, and prints candidates when none qualify. Runtime
  validation is pending.

- **`nodemigrate`: restore a standalone Pod that was live in the source.** In
  the upstream lane of run `36377385003`, return migration and Node readiness
  succeeded, but the `restartPolicy: Never` standalone Pod had become Failed
  with exit 137 during cutover. Importing its source spec over the same-name
  terminal object left it terminal. The protected export now marks Pods whose
  source phase was Running; import strips the marker before API writes and
  deletes/recreates a terminal same-name destination Pod with its UID
  precondition. A focused regression covers marker generation and terminal
  detection; migration verification is pending.

- **Five-node Docker preflight lost Cilium mount propagation after cp-1
  restart.** In run `36377385003`, kubelet/API and all five Nodes recovered,
  but Cilium remained unready because `/sys/fs/bpf` was not shared with the
  CNI mount namespace. The preflight now makes `/sys` shared before initial
  setup and restores `/sys`, bpffs, and `/run` propagation before re-enabling
  kubelet after restart. Shell validation passed; runtime verification is
  pending.

- **`nodemigrate`: transient admission failure aborts reference repair.** In
  branch-runtime run
  [36373995554](https://github.com/centerionware/not-k8s/actions/runs/36373995554),
  upstream return imported CRDs and began restoring objects, then failed while
  repairing owner references on a cert-manager CertificateRequest because its
  validating webhook Service was unavailable. The main object-apply phase
  retries transient API errors, but the reference-repair phase applied once
  and aborted. Reference repair now uses the same bounded transient retry
  policy. The existing classifier regression verifies retryable HTTP 500 and
  permanent HTTP 422 behavior; CI validation is pending.

- **Migration fixture: wait for CNI and DNS before workload probes.** In run
  `36373995554`, K3s return entered Cilium init-container reconciliation and
  Pod/Service traffic failed with `no route to host`. The `emptyDir` fixture
  then found a Running Pod whose container was exited and reported an exec
  error, obscuring the earlier datapath failure. Each migration checkpoint
  now waits for the Cilium DaemonSet and CoreDNS Deployment before workload
  assertions, and prints their state when they do not become Ready. Runtime
  validation is pending.

- **Five-node migration preflight: duplicated cp-1 restart hid the failing check.**
  Run `36373995554` passed the first kubeadm/Cilium recovery but stopped and
  restarted cp-1 a second time; the second combined wait failed without naming
  the failed predicate. The preflight now performs one failure-isolation and
  recovery cycle, logs kubelet/API/all-node/Cilium readiness separately, and
  captures cluster and node diagnostics on timeout. Shell validation passed;
  runtime verification is pending.

- **Migration CI: restarted five-node control plane was not fully ready before
  fixture setup.** In branch-runtime run
  [36371692313](https://github.com/centerionware/not-k8s/actions/runs/36371692313),
  the five-node kubeadm/Cilium recovery preflight passed, but later fixture
  installation used cp-1's API while it was still refusing connections. The
  preflight now waits for cp-1's kubelet and API, all five Ready Nodes, and the
  Cilium DaemonSet before migration begins. `bash -n` passed; rerun pending.

- **Migration fixture: PDB eviction assertion raced nginx rollout.** In the
  upstream lane of run `36371692313`, return import and replacement Node
  registration succeeded, but an nginx eviction succeeded even though the
  fixture had just observed `disruptionsAllowed=0`. Diagnostics show old and
  replacement nginx Pods during rollout. The fixture now waits for exactly one
  non-terminating Ready replica before validating the PDB and choosing the
  eviction target; an unexpected success prints the PDB, selected Pod, all
  matching Pods, and response for diagnosis. This remains a harness hypothesis
  until the focused migration rerun establishes the actual budget and Pod
  state at request time.

- **K3s return discovery and Cilium Service networking still fail.** Run
  `36371692313` reproduced all 20 destination-discovery timeouts after return
  API readiness; Cilium Service traffic also reports `no route to host`. The
  full cause and any required runtime change remain unverified.

- **`nodelet`: stale CRI records can make Pod exec/status select the wrong runtime object.**
  Run [36357521280](https://github.com/centerionware/not-k8s/actions/runs/36357521280)
  failed both single-node lanes' first `stage=nodestore` non-root `emptyDir`
  marker check (`CONTAINER_EXITED` in K3s; container missing in upstream).
  Code inspection found sandbox and container lookup trusting the first CRI
  list entry. The branch now selects a sandbox matching the current Pod UID
  when available, otherwise prefers a ready/newer sandbox; exec and
  reconciliation select a running container attempt, then the newest. Focused
  regressions cover stale sandbox UIDs, readiness/order, exited attempts, and
  wrong-name filtering. Quick-check [36359755288](https://github.com/centerionware/not-k8s/actions/runs/36359755288)
  passed at SHA `8f7df53e`. In run
  [36359977515](https://github.com/centerionware/not-k8s/actions/runs/36359977515),
  both source and first nodestore checkpoints passed. After failed return
  migrations and nodestore rollback, a stale task (`no running task found`) or
  missing Pod made the post-rollback `emptyDir` exec fail. The app-container
  reconciliation still selected the first name-matching CRI attempt; it now
  uses the same running/newest selection. Focused validation is pending.

- **Five-node CI: setup-script path mismatch after recovery.** Run
  [36359977515](https://github.com/centerionware/not-k8s/actions/runs/36359977515)
  passed the five-node kubeadm/Cilium readiness and control-plane-loss
  recovery checks. The preflight copied the hostpath helper to `/var/tmp`, but
  the five-node integration script still requested `/tmp` inside `cp-1` and
  failed with `cannot stat`. The coordinator now passes the configured helper
  path through to each fixture invocation. Shell validation passed; runtime
  verification is pending.

- **`nodemigrate`: return cutover does not recover the retained API with its
  workloads available.** In run 36359977515, K3s's 20 retained-API probes each
  timed out after ten seconds. The upstream return's first readiness probe got
  connection refused; object import then failed when the cert-manager
  validation webhook ClusterIP timed out. Both migration utilities restored
  nodestore, but post-rollback workload exec was also unavailable. The exact
  K3s API timeout cause remains unconfirmed; this is separate from the fixed
  selector compilation issue and needs a focused return-path fix.

- **`nodemigrate`: Cilium identity records conflict with destination allocation.**
  In branch-runtime run
  [36366505575](https://github.com/centerionware/not-k8s/actions/runs/36366505575),
  the exporter selected 21 source and later 27 destination `CiliumIdentity`
  objects. At `01:50:51Z`, the Cilium agent could not recreate the
  metrics-server identity because that identity object already existed. The
  parity harness already classifies `CiliumIdentity` as destination-rebuilt
  runtime state; the exporter now does the same while retaining Cilium policy
  objects. A focused regression covers the classification. Migration runtime
  confirmation is pending. This evidence shows an identity allocator conflict;
  it does not yet prove that it caused the later API discovery timeout or
  failed Service probes.

## Latest branch-runtime run findings

- **Strict CRD parity found changed numeric values after a full Kubernetes
  round trip.** Run [36402381606](https://github.com/centerionware/not-k8s/actions/runs/36402381606)
  reached `PASS stage=returned`, then reported the Traefik CRD schema boundary
  changing from `9223372036854775000` to `9223372036854776000` on two CRDs.
  These spellings represent different IEEE-754 `float64` values, so this is a
  real change and the parity assertion stays strict. The owner is the migration
  JSON transfer or nodeapiserver typed/protobuf storage path; isolate the first
  conversion that changes the value and add an HTTP-level regression. Relevant
  artifact: `/tmp/nodemigrate-artifacts-36402381606/kubernetes/nodemigrate-kubernetes.log`.

- **K3s return loses the Node after reporting it Ready.** In the same run,
  nodemigrate completed protected API import, waited for the replacement Node
  to become Ready, and returned success. Later returned-stage diagnostics
  recorded `Node ... Not Found`, including failed PV affinity lookups and
  EndpointSlice updates. Current logs do not prove which controller or process
  removed it. The owner is not yet identified; next run diagnostics record
  Node/Lease state and the K3s/kubelet journal when readiness fails. Artifact:
  `/tmp/nodemigrate-artifacts-36402381606/k3s/nodemigrate-k3s.log`.

- **Five-node source NetworkPolicy allow probe fails while CoreDNS crash-loops.**
  In run [36402381606](https://github.com/centerionware/not-k8s/actions/runs/36402381606),
  the Docker/kubeadm/Cilium fixture created the allow Job, but it did not reach
  its successful condition. CoreDNS Pods were `CrashLoopBackOff`; captured
  diagnostics did not include their container logs, so DNS is a correlated
  signal, not yet a confirmed cause. The harness now captures current and
  previous CoreDNS logs from cp-1 with its admin kubeconfig. Artifact:
  `/tmp/nodemigrate-artifacts-36402381606/docker/nodemigrate-docker-preflight.log`.

- **K3s return: full destination discovery still times out after core API readiness.**
  In branch-runtime run
  [36369153385](https://github.com/centerionware/not-k8s/actions/runs/36369153385),
  the direct Namespace readiness request succeeded, but all 20 ten-second
  `Discovery::run()` probes timed out. Cilium diagnostics from the same lane
  show Service ClusterIP and Pod-IP probes failing with `no route to host`,
  including API-backed workload endpoints. This points to the Cilium Service
  datapath as a likely shared factor, but the log does not identify which
  discovery request stalled, so the direct cause remains unconfirmed. Add
  per-endpoint discovery timing/status diagnostics before changing migration
  semantics or retry duration.

- **Upstream return: replacement deletes the live Node and never re-registers it.**
  In the same run, reverse import accepted all 55 CRDs and finished applying
  the API export. The returned node was Ready in snapshots just after service
  startup, then later snapshots had no Node and reported `no nodes available
  to schedule pods`. `NODEMIGRATE_REPLACE_NODE=true` is set by this fixture;
  `migrate_to_existing()` activates the retained service, imports state, then
  deletes the same-name Node and waits for readiness without restarting the
  kubelet. That sequence matches the observed Node disappearance. The branch
  now restarts the retained service after a Node deletion to trigger fresh
  registration. Earlier snapshots also show a Cilium agent initialization
  delay and Service probes failing `no route to host`, which remain separate
  networking signals to recheck after the registration fix.

- **Cilium identity skip is active but is not a migration pass.** Run
  [36369153385](https://github.com/centerionware/not-k8s/actions/runs/36369153385)
  confirms the exporter skipped 18–21 `CiliumIdentity` records in both
  directions while keeping declarative policies migratable. Both migrations
  still failed as described above. Identity allocator collision warnings also
  appeared during the upstream nodestore stage; their cause and connection to
  final node readiness have not been established. Keep the classification
  change, but do not treat it as a datapath fix.

## Current fixture finding

- **Branch runtime runs 36352851628 and 36355485146 exposed return-path defects.**
  Both passed forward migration and the nodestore object/CRD checkpoint. In
  the first K3s return, Cilium health binding failed with `address already in
  use` and Service/Pod IP traffic failed with `no route to host`; KPR was
  enabled and `kube-proxy` and `nodeproxy` were inactive. The branch now stops
  nodestore pod sandboxes and exact-identity Cilium processes before starting
  the retained cluster. Run 36355485146 confirms no source Cilium process
  remained and the duplicate health bind is gone, but Service traffic still
  fails and the node does not recover. The return export selected 30
  `CiliumEndpoint` objects and one `CiliumNode`; the branch now omits those
  Cilium-managed runtime records for destination reconciliation while
  preserving declarative Cilium policies. Focused quick-check passed in
  [36355485143](https://github.com/centerionware/not-k8s/actions/runs/36355485143);
  live confirmation is pending. The upstream return successfully recreated
  `migration-apps/migration-legacy-token` with destination credentials and
  completed API import, but the returned node was not Ready; its Cilium/runtime
  cause remains under investigation.

  The original five-node `node()` wrapper failure (`cp-1: executable file not
  found`) is fixed: run 36355485146 reached and passed the five-node Cilium
  preflight. The migration fixture then failed because its hostpath setup
  script was missing from `cp-1` after that node was restarted during
  preflight. The preflight now copies the script again after the restart,
  immediately before the five-node fixture. The multi-node migration path
  remains unverified. Artifacts are saved under
  `/tmp/nodemigrate-36355485146/`.

- **Five-node return fixture: kubeadm workers had no API admin kubeconfig.**
  The return coordinator supplied `/etc/kubernetes/admin.conf` to worker
  migrations, but `kubeadm join` does not install that control-plane credential
  on worker nodes. After the retained API recovered, the fixture now copies the
  recovered control-plane admin config to each worker with mode `0600` before
  asking nodemigrate to verify fresh Node registration. The shell syntax check
  passed in PR validation [36353488792](https://github.com/centerionware/not-k8s/actions/runs/36353488792)
  at SHA `1bcd76e3`; the active runtime run
  [36352851628](https://github.com/centerionware/not-k8s/actions/runs/36352851628)
  predates this fix, so its round-trip result cannot validate the correction.
  Runtime verification at the corrected SHA is pending.

- **`nodemigrate`: a new control-plane join could stay a Raft learner.** The
  five-node migration design joins each additional control plane to the
  existing nodestore cluster, but the previous code promoted membership only
  when replacing a known old member. A fresh control-plane join could
  therefore be Ready in Kubernetes while never voting in the datastore
  quorum. The migration now promotes the joined member after its Kubernetes
  Node becomes Ready; the existing replacement path still promotes before
  removing the old member. Focused `nodebootstrap` and `nodemigrate` tests
  passed at code SHA `2d98e05e` in
  [run 36350477684](https://github.com/centerionware/not-k8s/actions/runs/36350477684);
  the five-node runtime scenario remains pending.

- **`nodemigrate`: retained-API readiness discovery exceeds the probe deadline.**
  The original indefinite wait in run
  [36343008296](https://github.com/centerionware/not-k8s/actions/runs/36343008296)
  came from an unbounded discovery/list call inside the retry loop. A ten-second
  per-probe deadline fixed the unbounded wait, but branch-runtime run
  [36361932369](https://github.com/centerionware/not-k8s/actions/runs/36361932369)
  shows all 20 probes expiring while `ready()` performs discovery of every API
  group before listing Namespaces. The bounded retry then rolls back after five
  minutes. Readiness now checks the core Namespace API directly, avoiding full
  discovery; focused `nodemigrate` quick-check passed at SHA `2884eeaa` in
  [run 36363774087](https://github.com/centerionware/not-k8s/actions/runs/36363774087).
  A live return run is pending. Logs:
  `/tmp/nodemigrate-36361932369/nodemigrate-k3s-36361932369/nodemigrate-k3s.log`.

- **`nodemigrate`: destination API discovery can return 503 just after core API readiness.**
  In branch-runtime run
  [36363999391](https://github.com/centerionware/not-k8s/actions/runs/36363999391),
  the K3s return passed the direct core Namespace readiness probe but
  `Discovery::run()` returned HTTP 503 before namespace import, causing
  rollback. Initial destination discovery now retries for up to five minutes,
  with each probe capped at ten seconds and failures logged. Focused
  `nodemigrate` quick-check passed at SHA `801b3275` in
  [run 36366260390](https://github.com/centerionware/not-k8s/actions/runs/36366260390).
  The first quick-check at SHA `78d2869b` found a missing delay constant; it
  was added before the passing rerun. Live run
  [36366505575](https://github.com/centerionware/not-k8s/actions/runs/36366505575)
  confirms the direct readiness fix but all 20 bounded full-discovery probes
  timed out. The captured target state had Cilium agent and Envoy unhealthy;
  upstream again could not reach the cert-manager webhook. The bounded retry
  prevents another indefinite wait but does not fix destination Cilium
  recovery or make API discovery complete. Track Cilium/service datapath
  recovery as the current blocker.

- **`nodemigrate`: later cluster nodes reused the first node's protected
  export.** Forward and staged reverse control-plane paths loaded
  `source-export=` and wrote node-specific host-path/CNI recovery material
  under the shared export directory. Concurrent or sequential replacements
  therefore shared recovery state, contrary to the per-node recovery contract.
  The utility now makes a deep, mode-restricted local copy for each source
  control-plane or worker before using it; control-plane snapshots are added
  only to that node's copy, while workers keep their separate local-volume
  snapshot. The original remains available to later nodes. A focused unit
  regression checks that changing one node's copy cannot alter another copy or
  the source export and that per-node scheduling metadata is retained. The
  strengthened unit test passed at code SHA `f5bcadbb` in run
  [36349107771](https://github.com/centerionware/not-k8s/actions/runs/36349107771);
  migration workflow static validation passed in
  [36349107726](https://github.com/centerionware/not-k8s/actions/runs/36349107726).
  Multi-node runtime verification remains pending.

- **`nodemigrate`: kube-proxy and nodeproxy could both own Service
  routing when Cilium KPR is disabled.** The upstream lane in run
  [36335580680](https://github.com/centerionware/not-k8s/actions/runs/36335580680)
  used Cilium KPR=false; its target logs show a running kube-proxy Pod and
  active nodeproxy at the same time. The source kube-proxy DaemonSet was
  imported while nodemigrate selected nodeproxy solely from Cilium's KPR
  setting. Forward migration now detects the source
  kube-proxy DaemonSet (from the live API or protected export), preserves it,
  and passes `--proxy=none`; joined workers inspect the destination API and
  make the same choice. It rejects a source that simultaneously enables
  Cilium KPR and kube-proxy before cutover. The migration checkpoint now
  requires exactly one Service proxy owner, or Cilium KPR alone. The focused
  nodemigrate crate tests passed in
  [36347140648](https://github.com/centerionware/not-k8s/actions/runs/36347140648)
  and migration workflow validation passed in
  [36347140725](https://github.com/centerionware/not-k8s/actions/runs/36347140725),
  both at `7bad3b91`. Runtime verification remains pending; run 36335580680
  confirms the pre-fix conflict.

- **Migration workflow: the KPR option was dropped across `sudo`.** Although
  the workflow step set `NODEMIGRATE_CILIUM_KPR`, its explicit
  `sudo --preserve-env` list omitted that variable, so the root-run source
  installer and integration assertions silently used the default `false`.
  The preserved environment now includes the option. Workflow validation
  passed at `7bad3b91` in run 36347140725; a runtime lane using both KPR
  settings remains pending.

- **Nodemigrate integration fixture / five-node preflight ignored its KPR
  input.** Run
  [36343008296](https://github.com/centerionware/not-k8s/actions/runs/36343008296)
  passed the Docker preflight with `cilium_kpr=true`, but inspection found the
  preflight script still hard-coded `kubeProxyReplacement=false` and installed
  kube-proxy. The five-node portion therefore did not cover the requested KPR
  mode. The script now passes KPR into kubeadm and Helm, skips the kube-proxy
  add-on when enabled, and asserts the kube-proxy DaemonSet is absent. Shell
  syntax and PR validation passed at SHA `74068255`; enabled-KPR preflight
  [36345278305](https://github.com/centerionware/not-k8s/actions/runs/36345278305)
  also passed. Its artifact confirms all five nodes became Ready, kube-proxy
  was absent, and the cluster recovered after control-plane loss. This only
  verifies the five-node setup, not nodemigrate across that topology. Run
  36343008296 predates the fix and is not evidence for five-node KPR.

- **nodemigrate / Cilium initial control-plane bootstrap started nodeproxy with
  kube-proxy replacement enabled.** Code review while KPR-enabled migration
  run [36343008296](https://github.com/centerionware/not-k8s/actions/runs/36343008296)
  was active found that the migration disabled nodeproxy for joined worker
  bootstraps but omitted `--proxy=none` from the first nodestore bootstrap.
  At SHA `da0fab88`, nodemigrate now reads
  `kube-system/cilium-config` from the live source API or protected export and
  applies the same proxy-mode selection to initial bootstrap and post-cleanup
  reconciliation. The KPR integration fixture now fails if nodeproxy is active
  at the nodestore checkpoint. Focused coverage checks both protected-export
  parsing and nodebootstrap arguments; nodemigrate quick-check
  [36343511798](https://github.com/centerionware/not-k8s/actions/runs/36343511798)
  passed at code SHA `da0fab88`. KPR-enabled migration verification is pending.
  Run 36343008296 predates this fix and does not verify it.

- **Migration diagnostics / return Service routing failure lacked kube-proxy
  readiness evidence.** Upstream lane of run
  [36335580680](https://github.com/centerionware/not-k8s/actions/runs/36335580680)
  failed while importing three cert-manager resources: the retained API timed
  out reaching the webhook Service ClusterIP. Its early snapshot did not
  include kube-proxy DaemonSet or Pod readiness; later rollback logs show
  kube-proxy and nodeproxy both active while Cilium KPR was disabled. This
  confirms the competing-proxy bug above. The target snapshot also showed
  Cilium agent and Envoy unready and CoreDNS/cert-manager containers terminated
  with exit 137; the cause of Cilium startup failure remains unconfirmed. At
  `bb32d6c8`, the migration watcher now captures the
  kube-proxy DaemonSet desired/current/ready/available counts and Pod
  conditions alongside its existing Cilium and webhook probes. `bash -n`,
  `git diff --check`, the migration-watcher JSON diagnostic check, and commit
  subject validation passed locally. PR static validation
  [36339547744](https://github.com/centerionware/not-k8s/actions/runs/36339547744)
  and targeted nodemigrate checks
  [36339547796](https://github.com/centerionware/not-k8s/actions/runs/36339547796)
  passed at SHA `bb32d6c8`. A new migration lane must supply runtime evidence.
  No PV deletion was reported in run 36335580680.

- **Migration integration fixture / StatefulSet API default compared as a
  data change.** Run
  [36320456287](https://github.com/centerionware/not-k8s/actions/runs/36320456287)
  at SHA `ad861cdc` proved the source CSI `dev-dir` `/dev` volume is present on
  the target immediately after forward migration in both K3s and upstream
  lanes. Strict normalized API object parity also passed after the CSI fixture
  reinstall. The lanes then stopped because the application snapshot contained
  `spec.minReadySeconds: 0` on a target StatefulSet while the source omitted
  it; this is the API's zero default, not a changed workload setting. The
  current snapshot filter removes only StatefulSet `minReadySeconds: 0` and a
  focused check confirms a nonzero value remains significant. The five-node
  kubeadm/Cilium preflight passed but did not run nodemigrate. No PV, PVC, or
  backing-volume deletion was reported. Artifacts are at
  `/tmp/nodemigrate-36320456287-artifacts/`.

- **Migration harness / CSI driver redeployment changed a volume after
  migration.** Run
  [36318123614](https://github.com/centerionware/not-k8s/actions/runs/36318123614)
  at SHA `a95833a4` showed `default/csi-hostpathplugin`'s source `dev-dir`
  `/dev` mount missing only after the integration harness reinstalled CSI.
  Focused nodeapiserver schema-pruning and protobuf storage round-trip checks
  passed at [36319639827](https://github.com/centerionware/not-k8s/actions/runs/36319639827)
  and [36319953565](https://github.com/centerionware/not-k8s/actions/runs/36319953565).
  At `ad861cdc`, the migration test asserted the volume before reinstallation
  and strict API parity passed after it was restored. This was a fixture
  mutation, not a confirmed migration loss. PV/PVC identity and data checks
  passed; neither lane reached return in that run.

- **Migration harness / ServiceAccount token fixture was rejected before
  migration.** In run
  [36315595317](https://github.com/centerionware/not-k8s/actions/runs/36315595317),
  both source lanes failed because the fixture created a
  `kubernetes.io/service-account-token` Secret before adding its required
  `kubernetes.io/service-account.name` annotation. Kubernetes rejected the
  Secret; nodemigrate never ran, so this is not a migration utility failure or
  evidence about the token-preservation fix. The fixture now submits the
  ServiceAccount name and UID annotations in the initial create request. The
  five-node cluster preflight passed, but migration checkpoint and token
  behavior remain unverified. Logs are saved under
  `/tmp/nodemigrate-36315595317-k3s-artifact/` and
  `/tmp/nodemigrate-36315595317-kubernetes-artifact/`.

- **Migration harness / storage safety: StatefulSet fixture deleted a PVC.** In
  run [36304940296](https://github.com/centerionware/not-k8s/actions/runs/36304940296),
  `exercise_statefulset_scaling` deleted ordinal 1's claim after scaling the
  fixture down. The claim's hostPath CSI PV used reclaim policy `Delete`, and
  the CSI provisioner log confirms it deleted that test volume. This happened
  in fixture preparation before nodemigrate ran; the migration utility did not
  delete the PV. Even as test cleanup, deleting a workload claim in the
  migration checkpoint undermines the required data-preservation proof. The
  fixture now retains the ordinal 1 PVC, PV binding, and payload through
  scale-down and scale-up, and verifies the same claim UID, volume name, and
  data. This fix was exercised at both source and nodestore checkpoints in
  K3s and upstream Kubernetes in run
  [36306098280](https://github.com/centerionware/not-k8s/actions/runs/36306098280);
  all four storage assertions passed. The migration lanes later failed on
  unrelated object-preservation/parity checks, before completing round trips.

- **`nodemigrate`: legacy ServiceAccount token Secrets were skipped and their
  source JWTs cannot authenticate against a different cluster.** The importer
  now carries these Secrets, requests a destination-bound token for the
  imported ServiceAccount, and updates the token, target CA, namespace, and
  ServiceAccount UID annotation in the same Secret while preserving other
  Secret data. The integration fixture now checks token-authenticated allowed
  and denied API requests at source, nodestore, and returned-source stages.
  Focused export/patch tests and snapshot normalization checks are added.
  Targeted nodemigrate quick-check
  [36315451874](https://github.com/centerionware/not-k8s/actions/runs/36315451874)
  passed at SHA `17281efb42343ae2f934f681f57f031a59bc6016`. In run
  [36316498148](https://github.com/centerionware/not-k8s/actions/runs/36316498148)
  at `b69ef4e7`, allowed and denied token/RBAC probes passed at source and
  nodestore. No return-stage result is claimed because CSI parity stopped the
  lanes first.

## Latest runtime findings

- **`nodemigrate`: destination collisions retained fields outside the source
  object, and Node-owned references were lost.** Run
  [36311005956](https://github.com/centerionware/not-k8s/actions/runs/36311005956)
  at `42162ef2` passed PV/PVC bindings and data in both lanes, then found
  same-name differences in CoreDNS and a Node-owned Secret. Existing-object
  imports now use resource-version-checked full replacement; missing objects
  use create. After the replacement Node becomes Ready, nodemigrate restores
  Node owner references from the protected export using the source and
  destination Node UIDs. At `f9356dad`, nodemigrate quick-check
  [36313588814](https://github.com/centerionware/not-k8s/actions/runs/36313588814)
  passed; branch-runtime run
  [36313589064](https://github.com/centerionware/not-k8s/actions/runs/36313589064)
  passed source and nodestore stages in both lanes, including PV/PVC/data and
  all 119 source-discovered APIs. The prior CoreDNS/owner-reference mismatches
  were not reported. Strict parity then failed on fixture hostPath CSI paths
  changed when the target driver was reinstalled under `/var/lib/nodelet`, its
  extra source-stage mount, and regenerated `CiliumNode.spec.health.ipv4`.
  Neither lane reached return migration. Logs:
  `/tmp/nodemigrate-36313589064-k3s.log` and
  `/tmp/nodemigrate-36313589064-kubernetes.log`.

- **`nodelet`: CSINode registration omits and does not repair its Node owner
  reference.** The first branch-runtime run with strict target CSINode
  registration checks, [36308845021](https://github.com/centerionware/not-k8s/actions/runs/36308845021),
  passed source checks and reached the nodestore stage in both lanes, then
  failed because the hostpath `CSINode` had a registered driver but no
  `metadata.ownerReferences` entry for the destination Node. The existing
  nodelet create path explicitly skipped the Node UID lookup, and its update
  path only patched `spec.drivers`; this also left a same-name replacement
  vulnerable to a stale owner UID. Nodelet now fetches the current Node UID
  during CSI registration and sets the CSINode owner reference on both create
  and update. Focused nodelet quick-check
  [36311005893](https://github.com/centerionware/not-k8s/actions/runs/36311005893)
  passed at SHA `42162ef2`. Fresh branch-runtime migration
  [36311005956](https://github.com/centerionware/not-k8s/actions/runs/36311005956)
  confirmed current-Node ownership and passed source and nodestore PVC/PV
  binding and StatefulSet payload checks in both lanes. Both then failed strict
  source-object parity at nodestore, so neither reached return migration. The
  earlier run's source PVC/PV and payload checks passed; its nodestore payload
  checks did not run after the earlier owner-reference assertion failed.
  Upstream run [36310603619](https://github.com/centerionware/not-k8s/actions/runs/36310603619)
  at SHA `1dc1c2db` reproduced the failure: it reached the nodestore
  checkpoint with the hostPath CSI driver registered, but the CSINode still
  lacked the destination Node owner reference. The lane stopped before its
  nodestore storage assertions; K3s failed the same assertion. No PV deletion
  was reported. Target diagnostics also logged `NodeStageVolume` returning
  `NotFound` for the StatefulSet's existing CSI volume handle. This is a
  separate provider-state issue, not evidence that the PV or backing data was
  deleted, and needs a full storage checkpoint after the owner-reference fix.
  This SHA predates the fix.

- **`nodeapiserver` in regular release `v0.8.0`: Gateway API CRD CEL rules are
  rejected during import.** The latest-release migration run
  [36310054614](https://github.com/centerionware/not-k8s/actions/runs/36310054614)
  resolved `v0.8.0`; both the K3s and upstream source fixtures passed, but the
  release API rejected `gateways.gateway.networking.k8s.io` rules under
  `spec.infrastructure.annotations` and `labels` (including map-key
  comprehensions and regex validation) with CEL rule-cost estimation overflow.
  Both source PV/PVC/data checks passed, both protected exports were retained,
  and source rollback completed. Neither lane reached nodestore or the return
  migration; no PV deletion was reported. The later latest-runtime run
  [36332106663](https://github.com/centerionware/not-k8s/actions/runs/36332106663)
  reproduced the overflow and related map/object CEL type errors in both lanes;
  it also confirmed that source PV/PVC/data checks passed and rollback retained
  both exports. Branch-runtime run
  [36311005956](https://github.com/centerionware/not-k8s/actions/runs/36311005956)
  accepted the Gateway API CRDs and reached nodestore, showing this failure is
  specific to the v0.8.0 API baseline. The newer full migration behavior at
  `f9356dad` is under test in [36313589064](https://github.com/centerionware/not-k8s/actions/runs/36313589064).

Run [36307445772](https://github.com/centerionware/not-k8s/actions/runs/36307445772)
at SHA `d92e814911416403bba8b9cf02940fc23be1d140` passed the five-node
preflight and both utility/runtime builds. All StatefulSet PVC/PV/data checks
passed again. Both lanes reached `PASS stage=nodestore`, then strict source
object parity failed. The test's latest update classifies hash-specific system
ReplicaSets, controller leader Leases, CoreDNS bootstrap RBAC metadata, default
false PriorityClass values, and node-owned CSINode objects by their actual
lifecycle; it preserves strict checks for user Leases, custom RBAC, true
PriorityClass defaults, durable CSIDriver specs, and application rollout
history. It also checks that hostpath CSI has re-registered on every target
Node. Focused shell/filter checks pass; rerun these classifications against the
live scenarios before treating them as settled.

- **`nodeapiserver` / `nodescheduler`: Kubernetes 1.37 CSI scheduling field is
  dropped by the 1.34 API schema.** The upstream lane installs Kubernetes
  `v1.37.1`, while the target's upstream component remains `v1.34.11` and its
  vendored OpenAPI/protobuf schema is `release-1.34`. The strict parity failure
  identifies `/spec/preventPodSchedulingIfMissing` on
  `CSIDriver/hostpath.csi.k8s.io`; that field was added for Kubernetes 1.37's
  CSI-aware scheduling. The destination schema does not declare it, and the
  target object loses it. A narrow optional-field overlay now retains it in the
  1.34-based OpenAPI and protobuf schemas. `nodescheduler` watches CSIDriver
  dynamically so a newer typed field is not discarded, and enforces driver
  registration on a node when this flag is true. The first focused
  `nodeapiserver,nodescheduler` quick-check at SHA `de2b9ed9` ran 1,423 tests
  successfully and exposed another defect: pruning ignored `allOf` property
  declarations in the built-in CSIDriver OpenAPI schema, so the API-level
  regression failed. Schema pruning now merges property declarations across
  `allOf`, with both a focused merger regression and the actual CSIDriver
  schema test. The rerun is pending. This does not change the advertised base
  API version to 1.37 or claim full 1.37 compatibility. Migration run
  [36308845021](https://github.com/centerionware/not-k8s/actions/runs/36308845021)
  at SHA `7ba45556` predates the field and pruning fixes. The migration rerun
  at SHA `de2b9ed9` is still active.

Run [36306098280](https://github.com/centerionware/not-k8s/actions/runs/36306098280)
at SHA `39ceeb189ac619988ddc96815611b6e4d1848198` passed the five-node
Docker/kubeadm/Cilium preflight and built both the utility and branch runtime.
The corrected storage fixture passed at source and nodestore in both lanes.
K3s then failed because source-discovered `kube-system/metrics-server-77dbbf84b`
ReplicaSet was missing at nodestore. Upstream failed strict object parity;
remaining differences include changing Leases, CoreDNS RBAC defaults,
PriorityClass defaults, `CSIDriver.spec.preventPodSchedulingIfMissing`, and
`CSINode` owner references. Neither lane reached the return leg. Full logs are
saved at `/tmp/nodemigrate-36306098280/`.

- **Migration harness / system add-on ReplicaSet lifecycle:** the K3s target
  replaced the source `metrics-server` ReplicaSet (`metrics-server-77dbbf84b`)
  with a current ReplicaSet generated by its preserved Deployment. The test
  treated the old system-owned ReplicaSet identity as durable, although the
  existing Cilium/CoreDNS/local-path checks already classify such runtime
  children by their Deployment behavior. The snapshot filter now classifies
  metrics-server's old ReplicaSet the same way, and the stage check now waits
  for the metrics-server Deployment rollout and a current ReplicaSet. The
  focused snapshot-filter check passes; the migration rerun is pending. User
  workload ReplicaSets and ControllerRevisions remain in strict identity and
  spec parity checks.

Run [36299240279](https://github.com/centerionware/not-k8s/actions/runs/36299240279)
at SHA `20648debff0f7f338c3e37303bbaf367ffa449a4` passed the isolated
five-node Docker/kubeadm/Cilium preflight and both builds. K3s passed the
nodestore-stage workload checks, including Deployment scaling and StatefulSet
ordinal-0 data, then failed listable API-resource parity. Upstream passed the
same-stage workload/resource checks but failed strict source-object parity.
Neither lane reached return migration. The run did not delete a PV, PVC, or
backing volume.

- **`nodelet` / `nodebootstrap`: K3s metrics resource availability is still
  broken after cutover.** `nodes.metrics.k8s.io` and `pods.metrics.k8s.io` were
  absent from destination discovery, causing the source-discovered inventory
  check to fail. The APIService condition was `Available=True` with `all checks
  passed`, while metrics-server repeatedly logged an unknown-CA error scraping
  nodelet's `:10250/metrics/resource`. The relationship between failed scrapes
  and missing discovery is not yet proven. Nodelet currently issues a
  self-signed serving certificate. Fix and verify both discovery and returned
  node metrics without weakening TLS validation.
- **CSI provider state / migration fixture: a migrated hostPath CSI volume
  handle was missing from the driver's volume catalog.** Nodelet dynamically
  registered `hostpath.csi.k8s.io`; the driver then returned
  `volume id ... does not exist in the volumes list` for
  `state-migration-stateful-0`. The same run's StatefulSet ordinal-0 data
  assertion passed, so this failure is not evidence that the PV was deleted or
  that all fixture data was lost. Establish which mounted CSI PV generated the
  failed request, prove its provider data directory survives source shutdown
  and destination redeployment, and verify the same handle can be staged and
  read at both stages.
- **Deployment `/scale` regression is not reproduced.** The earlier run
  36297966919 failed its two-available-replicas assertion, but the same
  assertion passed in 36299240279 without a controller behavior change. Keep
  this as an unresolved intermittent result until the scale check passes
  repeatedly and the expected replica state is observed.
- **Upstream same-object parity remains incomplete.** The strict comparator
  still reports CoreDNS spec changes, ReplicaSet replicas, CSI driver and
  StatefulSet fields, CiliumNode data, Lease state, system RBAC/PriorityClass
  fields, and other paths in the saved log. Preserve the source behavior and
  state or regenerate target-owned fields explicitly; no blanket field
  normalization is justified.

## Previous branch-runtime findings from run 36297966919

Run [36297966919](https://github.com/centerionware/not-k8s/actions/runs/36297966919)
at SHA `d2eebede56084347cbc8eeba03fb12336bed7e6e` built the migration utility
and branch runtime and passed the Docker kubeadm/Cilium preflight. Both lanes
completed forward migration but failed before return migration.

- **`nodelet`: CSI staging failed during the K3s target checkpoint.** The
  target initially logged that `hostpath.csi.k8s.io` was not configured, then
  dynamically registered the driver at 05:59:47. After registration, the
  stateful PVC still logged failed `NodeStageVolume` mounts. The captured
  node-driver-registrar logs show successful CSI RPCs for the earlier staging
  path but do not identify why later staging failed; the run's final failure
  diagnostics do not include the error chain from `NodeStageVolume`. The
  driver endpoint therefore was eventually discovered, and the remaining
  storage failure is unresolved. Nodelet now includes the complete error chain
  in its volume-mount warning (commit `aa96d6b5`); targeted `nodelet` quick
  check [36299331009](https://github.com/centerionware/not-k8s/actions/runs/36299331009)
  passed, but the runtime run at `20648deb` predates this logging change.
  Verify the same bound PVC mounts and reads its original data at every
  checkpoint before treating the CSI issue as fixed.
- **`nodecontroller` / migration harness: Deployment `/scale` check failed at the K3s target
  checkpoint.** The target accepted `kubectl scale --replicas=2` and the
  fixture's rollout command returned success, but its follow-up assertion did
  not observe both requested replicas as available. The current failure
  diagnostic did not print Deployment/ReplicaSet state, so the exact
  controller state transition was not established in that run. The integration
  script now emits Deployment, ReplicaSet, Pod, and event state on this failure;
  the assertion passed in run 36299240279 without a controller behavior change.
  Treat the earlier failure as intermittent until repeated target runs pass.
- **`nodelet` / `nodebootstrap`: metrics-server cannot verify the target
  kubelet serving certificate.** Regenerating the aggregated-API trust bundle
  worked: the K3s `v1beta1.metrics.k8s.io` APIService reports `Available=True`
  with `all checks passed`, unlike the previous 401. metrics-server still logs
  `x509: certificate signed by unknown authority` when scraping the migrated
  nodelet's `:10250/metrics/resource` endpoint. Aggregated API availability is
  fixed; live node metrics remain unavailable until the serving certificate
  trust chain is made compatible and scrape results are verified.
- **`nodeapiserver` / runtime state: upstream strict API parity still fails.**
  The Kubernetes lane exposed all 119 source-discovered listable resources,
  passed target workload checks, then failed same-identity object fingerprints
  in CoreDNS, ReplicaSets/StatefulSets, Leases, PriorityClasses/system RBAC,
  CSIDriver/CSINode, and CiliumNode. These remain differences, not waived
  normalizations; no return comparison ran. Per-field paths and hashes are in
  `/tmp/nodemigrate-36297966919-kubernetes.log`.

## Branch-runtime findings from runs 36293700293 and 36294950798

Run [36293700293](https://github.com/centerionware/not-k8s/actions/runs/36293700293)
at SHA `ad2ee6a016993c43effeab3d1aa83e9f6cf40f5e` passed the five-node
Docker/kubeadm/Cilium preflight and both `nodemigrate` and branch-runtime
builds. The nodelet fix passed targeted quick-check
[36293700468](https://github.com/centerionware/not-k8s/actions/runs/36293700468).
Both migration lanes passed the new non-root, read-only-rootfs `emptyDir` write
probe at source and nodestore checkpoints, confirming that nodelet now gives
emptyDir roots kubelet's writable mode 0777.

The K3s lane at `36293700293` lacked `nodes.metrics.k8s.io` and
`pods.metrics.k8s.io` at nodestore: metrics-server was Ready, but APIService
discovery returned HTTP 403 because the availability probe did not present the
front-proxy identity. The fix gives the probe the upstream
`system:kube-aggregator` user, `system:masters` group, and configured
proxy-client certificate. Focused nodeapiserver quick-check
[36294951013](https://github.com/centerionware/not-k8s/actions/runs/36294951013)
passed. In migration run
[36294950798](https://github.com/centerionware/not-k8s/actions/runs/36294950798),
K3s APIService discovery then reported `Available=True`, reason `Passed`,
message `all checks passed`, confirming the 403 was fixed. Both fixture lanes
then stopped before migration because the harness passed large field-fingerprint
JSON through a `jq --argjson` command-line argument and hit `Argument list too
long`. The harness now streams normalized objects through stdin and keeps
field-level hashes out of the semantic-state snapshot. Focused shell and
inventory checks pass; another migration run is pending.

The upstream lane passed discovery and reached the object parity check, where
same-identity normalized source digests still differ. Affected examples include
`Deployment/coredns`, same-name ReplicaSets and StatefulSets, the hostPath CSI
driver, `CiliumNode`, `CSINode`, Leases, CoreDNS RBAC, and system
PriorityClasses. These remain failures; no source object is accepted as
rewritten by target runtime without evidence. The previous comparator also
treated target-only runtime objects as lost source state. Its current
worktree revision compares every source identity and digest while reporting
target-only identities independently, including when source objects have
changed. It now records hashed per-field fingerprints and reports only changed
field paths, never object or Secret values. Focused inventory-script checks
pass. Run `36294950798` did not reach object parity because of the harness
argument-size failure above; neither lane completed a return migration or
parity gate.

## Branch-runtime run 36285968999

Run [36285968999](https://github.com/centerionware/not-k8s/actions/runs/36285968999)
at SHA `879a0a618efe933d084023300460128da446da17` completed with both K3s and
upstream lanes failing at the target checkpoint. Both lanes built the utility
and branch runtime, completed migration, passed API readiness, and then got
HTTP 404 from the target for the fixture's PDB-protected Pod eviction request.
K3s applied all 59 CRDs; no return leg or parity comparison ran. Full artifacts
are under `/tmp/nodemigrate-36285968999-artifacts/`.

Confirmed defect and branch work:

- **`nodeapiserver`: missing Pod eviction REST handler.** The listener parsed
  `POST /api/v1/namespaces/{namespace}/pods/{name}/eviction` as a create
  subresource, but dispatch had no handler. Existing NodeRestriction logic
  already covered node-originated eviction, and nodecontroller computes PDB
  status, but neither was connected to an eviction endpoint. A handler now
  validates `policy/v1 Eviction`, runs NodeRestriction, matches PDB label
  selectors, returns 429 when matching status allows no disruptions, and
  otherwise requests graceful deletion. Focused tests cover selector matching
  and missing/zero/positive allowance values. Targeted quick-check
  [36287345872](https://github.com/centerionware/not-k8s/actions/runs/36287345872)
  passed. Migration run [36287345921](https://github.com/centerionware/not-k8s/actions/runs/36287345921)
  advanced beyond the PDB 429 assertion in both lanes, confirming the missing
  endpoint is fixed for this fixture. The current positive-allowance path does
  not reserve the PDB allowance atomically, so concurrent evictions remain a
  known gap.
- **`nodecontroller`: PV reclaim policies were not implemented.** At both
  nodestore checkpoints, deleting the temporary StatefulSet ordinal-1 PVC left
  its dynamically provisioned `Delete`-policy PV present for the full five
  minute fixture deadline. `pv_binder` explicitly documented that it did not
  handle release/reclaim. A first implementation deleted the PV after checking
  for stale claim UIDs, but run [36288813404](https://github.com/centerionware/not-k8s/actions/runs/36288813404)
  still failed both lanes. The hostPath CSI external-provisioner log explains
  why: it refused `DeleteVolume` while the terminating PV phase was still
  `Bound` (`shouldDelete is false: PersistentVolumePhase is not Released`). The
  current worktree now marks the PV `Released` before issuing a UID/resource
  version-preconditioned delete. This path is ordinary reclaim behavior after
  an explicit PVC deletion in the fixture; it is not part of migration. The
  migration path must preserve source PVs, PVCs, backing storage, and data.
  The first quick-check of this ordering fix failed compilation because the
  claimRef borrow crossed the PV update; its identity fields are now copied
  before mutation, and focused `nodecontroller` quick-check passed at
  [36290302028](https://github.com/centerionware/not-k8s/actions/runs/36290302028).
  Runtime verification is pending. Migration preserves the source PV/PVC/data;
  the reclaim check deletes only the explicitly removed temporary claim's PV.
  Artifacts:
  `/tmp/nodemigrate-36288813404-artifacts/nodemigrate-{k3s,kubernetes}-36288813404/`.

## Release-backed run 36279843865

Run [36279843865](https://github.com/centerionware/not-k8s/actions/runs/36279843865)
used branch SHA `d609ec4d66dfce13283d0ee4c1d39a5ecce10d8d` and fetched the
exact regular runtime asset `notk8s-0.8.0-linux-x86_64-release`. The five-node
Docker/kubeadm/Cilium preflight passed and both targeted `nodemigrate` release
builds passed. Neither migration lane reached parity or a completed round trip;
both rolled back and retained their protected exports.

Confirmed failures:

- **`nodeapiserver`: CRD CEL map/object parity.** Both source lanes failed to
  create the same three Gateway API CRDs: `gateways.gateway.networking.k8s.io`,
  `listenersets.gateway.networking.k8s.io`, and
  `tlsroutes.gateway.networking.k8s.io`. The server's CRD CEL type checker
  rejected upstream rules with errors such as `comprehension requires list or
  map range, got object` and `size requires string, bytes, list, or map, got
  object`; map key cost estimation also saturated to `u64::MAX`. This is
  a confirmed CEL compatibility defect in `nodeapiserver`, not a CRD migration
  filtering issue. Fix must preserve upstream validation behavior while
  correctly typing/evaluating Kubernetes map fields and estimating their
  bounded costs. The branch now covers map-shaped `additionalProperties` in
  CEL type checking and passes its focused quick-check; release-backed runs
  still fail because their target is v0.8.0. Branch-runtime migration evidence
  is pending.
- **`nodemigrate`: import dependency ordering.** The upstream lane tried
  `Pod/migration-standalone` before its referenced cluster-scoped
  `PriorityClass/migration-priority`; upstream admission returned 403. The
  importer ranked CRDs and StorageClasses specially but left PriorityClasses
  in API discovery order with workloads. The branch now ranks PriorityClass
  before workloads and adds a focused test. Quick-check passed, and the
  release-backed rerun no longer reports this 403.
- **`nodeapiserver`: CertificateRequest webhook and CSR decoding failures.**
  The upstream lane's cert-manager `CertificateRequest` apply failed because
  invocation of `webhook.cert-manager.io` was unavailable. The CSR write
  failed while decoding `io.k8s.api.certificates.v1.ExtraValue` as an object.
  These need branch-runtime diagnosis: the first needs service reachability and
  webhook readiness traced during import, and the second must confirm whether
  the API server's existing CSR `ExtraValue` protobuf handling fixes the 0.8.0
  failure. The decoder-focused unit test passes in quick-check.
- The remaining transient Gateway objects were unavailable because the
  corresponding Gateway API CRDs were rejected. These are downstream effects
  of the CEL defect, not an independent importer retry failure.

Artifacts: `/tmp/nodemigrate-36279843865-artifacts/nodemigrate-{k3s,kubernetes}-36279843865/`.
The run did not pass the required K3s+Cilium and three-control-plane/two-worker
round-trip gates.

## Release-backed rerun 36283329273

Run [36283329273](https://github.com/centerionware/not-k8s/actions/runs/36283329273)
used fix SHA `786e95577686f1fd5ed08a94f76ade48861314b4` and the exact regular
`v0.8.0` runtime. The Docker five-node kubeadm/Cilium preflight passed in
7m32s. Targeted Rust quick-check
[36283329275](https://github.com/centerionware/not-k8s/actions/runs/36283329275)
passed for `nodeapiserver` and `nodemigrate`. The import-order fix is reflected
in the run: the earlier `Pod/migration-standalone` PriorityClass 403 no longer
appears. Both lanes still failed against the released runtime on the known
Gateway API CRD CEL gap. Each lane rolled back, verified the source service/API
recovered, and retained the protected export. No semantic parity or return
migration passed.

Upstream lane additional failures: one cert-manager CertificateRequest received
an internal error because invocation of `webhook.cert-manager.io` could not
send to its service URL; one CSR write also returned HTTP 500. This run's CSR
response did not preserve the internal decoder cause, so the `ExtraValue`
decode diagnosis from run 36279843865 remains a working hypothesis pending a
branch-runtime reproduction. Two Gateway objects remained unavailable as a
consequence of the rejected CRDs. Full logs:
`/tmp/nodemigrate-36283329273-k3s/nodemigrate-k3s.log` and
`/tmp/nodemigrate-36283329273-k8s/nodemigrate-kubernetes.log`.

The release-backed run confirms the v0.8.0 baseline, not the branch's
`nodeapiserver` CEL fix. Next runtime validation must use the branch runtime to
exercise that fix and then continue to semantic parity.

## Latest diagnostic update

Migration run [36258945785](https://github.com/centerionware/not-k8s/actions/runs/36258945785)
identified a timing defect in `nodemigrate` import retry policy: in the
K3s+Cilium lane, an otherwise accepted CertificateRequest was retried only
five times at three-second intervals while cert-manager's webhook was still
starting. The current branch work retries transient API/webhook and transport
failures for up to five minutes, immediately surfaces permanent rejections,
and adds a focused classifier test. Script and runtime validation are pending.

The same run reached the upstream target checkpoint but the fixture comparator
treated destination-generated `kube-root-ca.crt` annotation metadata and
optional nested PVC-template TypeMeta as durable differences. The CA contents
are independently verified against the active API CA. A dedicated application
snapshot filter now excludes only that regenerated root CA ConfigMap and
removes only optional `apiVersion`/`kind` from PVC templates. Normalizer checks
pass locally, and targeted `nodemigrate` quick-check
[36260417435](https://github.com/centerionware/not-k8s/actions/runs/36260417435)
passed at `8c79470de60f288fc113db7b7b8c45da048b6ad7`. Full migration rerun
[36260417450](https://github.com/centerionware/not-k8s/actions/runs/36260417450)
passed source and nodestore target checks in the upstream lane but failed the
return import after five minutes: Certificate, CertificateRequest, and
ClusterIssuer requests timed out reaching the webhook ClusterIP, and a PV
request returned HTTP 404 despite the advertised resource. Neither error's
root cause is confirmed. The protected export remains retained, but no
returned-source checkpoint or round-trip parity passed. K3s is still running.
The saved upstream artifact identifies the affected PV as
`pvc-af0446fa-fa48-4e2b-86ca-7d98bb3843ce`; its NotFound status has no group,
kind, or causes, and this SHA predates the request-path diagnostic, so the
failed route cannot be reconstructed from that response. The return error also
reported nodestore remained stopped. The later reverse-cutover rollback fix
addresses this recovery state but has not yet been exercised by a runtime
migration.
Commits `058fafa8` and `434c9acb` add return-target webhook/endpoint probes and
the destination API path to restore errors. Focused nodemigrate tests
[36263787654](https://github.com/centerionware/not-k8s/actions/runs/36263787654)
and migration-workflow validation
[36263787652](https://github.com/centerionware/not-k8s/actions/runs/36263787652)
passed at `434c9acb`; runtime validation of these diagnostics is pending.
Commit `3f7c8873` also fixes a separate recovery defect: a failed reverse
cutover now stops the partial retained target, restores backed-up local volume
payloads, and restores the prior nodestore service state. The integration
harness now checks nodestore API readiness, protected export retention, and
fixture behavior after a failed return leg. Focused nodemigrate tests and
migration-workflow validation passed on `3f7c8873` in
[36264669564](https://github.com/centerionware/not-k8s/actions/runs/36264669564)
and [36264669587](https://github.com/centerionware/not-k8s/actions/runs/36264669587).
Runtime verification is pending.

Commit `c3be42bc` narrows import 404 retries to versions of custom resources
declared by source CRDs. A 404 or missing-discovery error for a built-in API
resource, such as the failing PersistentVolume, now fails immediately with the
destination request path instead of consuming the five-minute transient retry
window. Focused nodemigrate tests [36266754129](https://github.com/centerionware/not-k8s/actions/runs/36266754129)
and migration workflow validation [36266754130](https://github.com/centerionware/not-k8s/actions/runs/36266754130)
passed. This improves failure time and diagnosis; it does not establish why the
PV route returned 404 or prove successful restoration. Runtime rerun remains
pending, and run `36260417450` still reported K3s in progress at
2026-09-26 19:40 UTC.

## Latest diagnostic update

Migration run [36251890971](https://github.com/centerionware/not-k8s/actions/runs/36251890971)
used SHA `16c90a9721dd1f0bbcdc1a11723d7438d173b49f`. The new SPDY
port-forward handling was confirmed in both target lanes: both Gateway probes
reached nginx and returned HTTP 200. Ingress returned HTTP 404 in both lanes.
`kubectl describe` showed `IngressClass: traefik` but no host rules; raw JSON
was not captured. No return migration or semantic parity checkpoint passed.
Follow-up run [36253413938](https://github.com/centerionware/not-k8s/actions/runs/36253413938)
captured the target object in both lanes: the host survived but nested
`http.paths` and its backend were absent, while `IngressClass.spec.controller`
was correct. This confirms a `nodeapiserver` protobuf storage codec bug. The
initial diagnostic check only ran on source; it is now moved into every stage
check. The codec fix and regression are pushed as `4984eb2e`; focused
`nodeapiserver` quick-check [36255128063](https://github.com/centerionware/not-k8s/actions/runs/36255128063)
passed. Dedicated migration rerun
[36255128061](https://github.com/centerionware/not-k8s/actions/runs/36255128061)
is in progress after its Docker preflight passed. PR validation
[36253405752](https://github.com/centerionware/not-k8s/actions/runs/36253405752)
passed. Logs:
`/tmp/nodemigrate-36253413938-artifacts/nodemigrate-{k3s,kubernetes}-36253413938/`.

Earlier, run [36250505911](https://github.com/centerionware/not-k8s/actions/runs/36250505911)
and focused `nodelet` quick-check
[36250504453](https://github.com/centerionware/not-k8s/actions/runs/36250504453)
confirmed static hostPath and CSI PVC data reads in both lanes. It exposed the
`nodeapiserver` SPDY port-forward compatibility bug, which was fixed and is
runtime-confirmed above.

The preceding nodelet work resolves bound `hostPath` and `local` PV sources
directly, validating declared hostPath types and requiring local PV paths to
be directories. It passed the focused `nodelet` quick-check and the migration
data-read assertion in both lanes above. A new confirmed `nodeapiserver`
port-forward compatibility bug is fixed and runtime-confirmed above. The
Ingress 404 has no confirmed component owner yet.

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
| The target-state watcher merged `kubectl` stderr into the JSON stream before piping it to `jq`. During API or optional add-on failures, error text produced repeated `jq` parse errors and obscured the actual request failure. | `.github/scripts/nodemigrate-integration.sh` target-state diagnostics | Capture request stderr separately, pass only stdout to `jq`, and encode failed requests or invalid responses as structured diagnostic JSON. Add a focused regression for failed requests, valid JSON with stderr warnings, jq arguments, and malformed responses. | Confirmed in upstream artifact [36335580680](https://github.com/centerionware/not-k8s/actions/runs/36335580680): 226 `jq: parse error: Invalid numeric literal` lines, from a pre-fix run at SHA `f186930d`. The fix is committed and pushed at `deaed970`; migration workflow validation [36338206523](https://github.com/centerionware/not-k8s/actions/runs/36338206523) passed. Local `bash -n`, the focused mocked diagnostic regression, and `git diff --check` passed. A branch-runtime migration at the fix SHA is still required to verify the errors are gone. |
| A return migration can remain in an external service, filesystem, API, import, or node-readiness operation without identifying its current phase. K3s run [36321804254](https://github.com/centerionware/not-k8s/actions/runs/36321804254) ended at the 180-minute workflow timeout; after its return export message, the log did not show which subsequent operation was active. The timeout alone does not establish the root cause. | `nodemigrate` reverse-migration orchestration | Emit a progress boundary before and after stopping nodestore, snapshotting local PV payloads, starting the retained service, waiting for API readiness, importing the API export, and waiting for the returned Node. Keep readiness and rollback behavior unchanged; use the next run to identify the blocking operation before selecting a behavioral fix. | Instrumented in SHA `5ce3f3d6`. The completed K3s log `/tmp/nodemigrate-36321804254-k3s/nodemigrate-k3s.log` confirms the export message and missing phase boundary through timeout; no PV deletion was reported and source/nodestore storage checks passed. Focused `nodemigrate` crate checks passed in [36334107161](https://github.com/centerionware/not-k8s/actions/runs/36334107161); migration-workflow static validation passed in [36334107153](https://github.com/centerionware/not-k8s/actions/runs/36334107153). Both were tested at the exact fix SHA. The new phase messages still need a runtime migration run. |
| The migration target-state watcher repeatedly exec'd `cat` in Cilium and cert-manager containers to inspect the projected service-account CA. Distroless images may not provide `cat`, so the probe emitted repeated CRI errors throughout a long return-migration wait and obscured the useful state. | `.github/scripts/nodemigrate-integration.sh` diagnostics | Run the probe only when emitting a state snapshot, key its result by Pod UID/container, cache both success and unavailable results, and suppress raw exec errors while recording an unavailable fingerprint. | Reproduced in the completed K3s artifact for [run 36321804254](https://github.com/centerionware/not-k8s/actions/runs/36321804254), where target CA exec failures repeated through the 180-minute timeout. The fix is in SHA `d0f7e048`. A mocked distroless-container smoke check confirmed exactly one exec per Pod UID/container across repeated captures and no emitted exec error; `bash -n` and `git diff --check` passed. The PR migration-script validation [36333313151](https://github.com/centerionware/not-k8s/actions/runs/36333313151) passed, and focused `nodemigrate` crate checks [36333313146](https://github.com/centerionware/not-k8s/actions/runs/36333313146) passed at the same SHA. This fixture diagnostic did not cause the migration timeout, whose root cause remains unverified. |
| The source-stage PDB regression never exercised the eviction API because the fixture passed a YAML body to `kubectl create --raw`, which forwards raw request bytes and requires JSON. Both lanes stopped at this assertion before invoking nodemigrate. | `.github/scripts/nodemigrate-integration.sh` fixture | Build a `policy/v1` Eviction request with `jq -cn` and send the JSON body to the raw eviction endpoint; keep asserting a PDB-protected HTTP 429 and Pod readiness afterward. | Confirmed in both lanes of [run 36279222996](https://github.com/centerionware/not-k8s/actions/runs/36279222996): the API rejected the YAML bytes with `invalid character 'a' looking for beginning of value`. The worktree now emits compact JSON. `bash -n`, `git diff --check`, and the jq request-shape check pass; the release-backed rerun is pending. Logs: `/tmp/nodemigrate-36279222996-artifacts/nodemigrate-{k3s,kubernetes}-36279222996/`. |
| A failed nodestore-to-Kubernetes/K3s cutover left nodestore stopped after the retained target started or API import failed. The upstream return leg in [36260417450](https://github.com/centerionware/not-k8s/actions/runs/36260417450) reproduced it: import exhausted five minutes and reported that nodestore remained stopped, while the protected export was retained. | `nodemigrate` reverse-cutover orchestration and `service` rollback | Stop the partially started retained target, including static pods and Cilium processes where applicable; restore saved hostPath/local payloads; then restore the prior nodestore service stack. Keep nodestore stopped if the retained target cannot be safely stopped. Add ordering/failure unit tests and verify recovered API, export, and fixture state in the migration harness. | Implemented in `3f7c8873`. Focused nodemigrate tests and migration-workflow validation passed at [36264669564](https://github.com/centerionware/not-k8s/actions/runs/36264669564) and [36264669587](https://github.com/centerionware/not-k8s/actions/runs/36264669587). The original run did not verify recovery after the failure; runtime verification remains pending. |
| `IngressRule.http` disappeared when a migrated Ingress was persisted. Go embeds `IngressRuleValue`, flattening `http` in JSON, while the protobuf codec only encoded declared top-level `IngressRule` fields; reads therefore returned each host rule without its HTTP paths or backend. | `nodeapiserver` Kubernetes protobuf storage codec | Treat `IngressRule.ingressRuleValue` as an inline embedded field for networking/v1, networking/v1beta1, and extensions/v1beta1 in protobuf encoding and decoding. Add a v1 Ingress codec round-trip regression and assert the full ingress spec at every migration checkpoint. | Confirmed in both branch-runtime lanes of [36253413938](https://github.com/centerionware/not-k8s/actions/runs/36253413938): target JSON retained `spec.rules[].host` but had no `http`, while the IngressClass controller was correct. The nested backend omission matches the codec's unhandled Go-embedded proto field. The fix and regression are in the worktree; `nodeapiserver` quick-check and dedicated migration rerun are pending. Logs: `/tmp/nodemigrate-36253413938-artifacts/nodemigrate-{k3s,kubernetes}-36253413938/` |
| `kubectl port-forward` over SPDY may omit the initial `ports` query parameter; Kubernetes sends the remote port in each SPDY stream header. `nodeapiserver` rejected the valid connect request before proxying it to nodelet, preventing Ingress and Gateway API data-plane checks after migration. | `nodeapiserver` Pod port-forward request translation | Allow an empty query port list on the SPDY path and proxy the upgrade unchanged. Keep supplied `port`/`ports` values normalized for WebSocket requests. Add a regression for a valid port-forward target without query ports. | Confirmed in both lanes of [36250505911](https://github.com/centerionware/not-k8s/actions/runs/36250505911), where the target returned `at least one port is required for port-forward`. Focused `nodeapiserver` quick-check [36251889485](https://github.com/centerionware/not-k8s/actions/runs/36251889485) passed. In [36251890971](https://github.com/centerionware/not-k8s/actions/runs/36251890971), both post-fix Gateway probes returned HTTP 200 with nginx content, confirming the SPDY path works. The separate Ingress 404 is root-caused in the adjacent tracker row. |
| During migration, nodelet requests a second stage path for a volume that kubelet already staged. Nodelet's old path used the raw volume handle under `/var/lib/nodelet/csi`; kubelet uses the SHA-256 handle under `/var/lib/kubelet/plugins/kubernetes.io/csi`. The CSI provider returns `FailedPrecondition: already staged`, leaving workloads Pending. | `nodelet` CSI staging and `nodebootstrap` migration service configuration | Use kubelet-compatible driver/hash/globalmount layout, detect the active source root from mount state, pass it into the generated nodelet service, and expose the preserved staging mount to the replacement CSI Pod. Validate an explicit root against active stages. | Confirmed in [36243049356](https://github.com/centerionware/not-k8s/actions/runs/36243049356). Implemented in `4014685a` and `fc2e5082`; quick-check [36246005553](https://github.com/centerionware/not-k8s/actions/runs/36246005553) passed. Migration run [36245509026](https://github.com/centerionware/not-k8s/actions/runs/36245509026) then confirmed `NodeStageVolume` and `NodePublishVolume` succeeded in both lanes. Cross-provider or different-node CSI data transfer remains unverified. Logs: `/tmp/nodemigrate-36245509026/artifacts/`. |
| Newly created batch/v1 Jobs did not receive Kubernetes-generated selector labels. The Job controller created Pods, but `kubectl logs job/...` selected unrelated Pods. | `nodeapiserver` Job create defaulting for POST and create-on-apply | After assigning request identity and UID, generate upstream legacy and prefixed Job/template labels plus UID-based `spec.selector.matchLabels`; preserve manual selectors. | Confirmed in both lanes of [36245509026](https://github.com/centerionware/not-k8s/actions/runs/36245509026). Fixed in `bd3b4f8f`; focused quick-check [36247102622](https://github.com/centerionware/not-k8s/actions/runs/36247102622) passed. Both lanes of [36247102741](https://github.com/centerionware/not-k8s/actions/runs/36247102741) passed the CronJob log-selection assertion. |
| A migrated PVC can retain `pv.kubernetes.io/bind-completed` and `spec.volumeName` after export strips controller-owned `status`. The PV binder treated the stale marker as complete and never restored `status.phase=Bound`, leaving the static claim Pending. | `nodecontroller` persistent-volume binder; `nodemigrate` status sanitization is the import trigger | Treat the claim as fully bound only when volumeName, bind-completed annotation, and status phase `Bound` are all present. Reconcile incomplete imported claims through the existing prebound-PV path. Add regression coverage for a stale marker without status. | Confirmed in both lanes of [36247102741](https://github.com/centerionware/not-k8s/actions/runs/36247102741). Fixed in `b14185a1`; focused `nodecontroller` quick-check [36248706148](https://github.com/centerionware/not-k8s/actions/runs/36248706148) passed, and migration [36248706224](https://github.com/centerionware/not-k8s/actions/runs/36248706224) observed both imported claims reach `Bound`. |
| Nodelet resolved PVC-backed volumes only when their bound PV had a CSI source. A claim bound to a static `hostPath` or `local` PV stayed Pending indefinitely despite a valid binding. | `nodelet` PVC volume resolution | Resolve bound hostPath and local PV paths directly, validating hostPath types and requiring local PV paths to be directories. Preserve CSI resolution for CSI PVs. Add focused source tests and migration data-read coverage. | Confirmed in both lanes of [36248706224](https://github.com/centerionware/not-k8s/actions/runs/36248706224): PVCs became Bound, then `migration-data-check` timed out because the hostPath PV did not mount. Fixed in `4d28a588`; `nodelet` quick-check [36250504453](https://github.com/centerionware/not-k8s/actions/runs/36250504453) passed, and migration run [36251890971](https://github.com/centerionware/not-k8s/actions/runs/36251890971) confirmed static hostPath and CSI data reads in both lanes. |
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
| The kubeadm/Cilium preflight initially timed out during the Cilium Helm wait without cluster diagnostics, preventing attribution of the readiness failure. | `.github/scripts/nodemigrate-docker-preflight.sh` (five-node CI harness) | On Cilium installation or readiness failure, capture Nodes, all Pods, recent events, Cilium agent/operator descriptions and current/previous logs, plus kubelet/containerd journals and CRI state from every node. | In [run 36272793413](https://github.com/centerionware/not-k8s/actions/runs/36272793413), five-node kubeadm membership completed, then Helm timed out waiting for Cilium. Diagnostics added in commit `1f46f848` were exercised in that run and exposed the `/sys/fs/bpf` mount-propagation error below. |
| Cilium could not finish starting in the Docker-simulated kubeadm nodes because nested containerd successively rejected mount propagation for Cilium's BPF, cgroup, and network-namespace paths. | `.github/scripts/nodemigrate-docker-preflight.sh` (five-node CI harness) | Keep bpffs directly mounted and shared at `/sys/fs/bpf`, mount and share cgroup2 at `/run/cilium/cgroupv2`, and make the node-local `/run` mount recursively shared for Cilium's `/var/run/netns` volume. Preserve per-node namespaces and BPF pin isolation. | Confirmed by diagnostics in runs [36273723872](https://github.com/centerionware/not-k8s/actions/runs/36273723872), [36274980364](https://github.com/centerionware/not-k8s/actions/runs/36274980364), [36276280017](https://github.com/centerionware/not-k8s/actions/runs/36276280017), [36277204858](https://github.com/centerionware/not-k8s/actions/runs/36277204858), and [36278041301](https://github.com/centerionware/not-k8s/actions/runs/36278041301). The direct bpffs mount cleared its error, then shared cgroup2 cleared its error; sharing `/run` cleared the netns propagation error. The five-node Cilium preflight passed in [36278826637](https://github.com/centerionware/not-k8s/actions/runs/36278826637), including control-plane loss and recovery. This resolves the test-harness blocker; it does not verify migration behavior. |
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

| The five-node Docker kubeadm probe could not pull images after switching to containerd's native snapshotter because containerd 2.2.1 had no image unpack platform configured for that snapshotter. | `.github/scripts/nodemigrate-docker-preflight.sh` (five-node CI harness) | Add an explicit Transfer API `unpack_config` entry mapping `linux/amd64` to the native snapshotter, and assert both fields before restarting containerd. | Confirmed by containerd logs in [run 36271904146](https://github.com/centerionware/not-k8s/actions/runs/36271904146): image pulls warned `Unpack configuration not supported, skipping` and failed with `no unpack platforms defined`. Runs [36272360238](https://github.com/centerionware/not-k8s/actions/runs/36272360238) and [36272577668](https://github.com/centerionware/not-k8s/actions/runs/36272577668) then failed at the nested-shell lookup before restarting containerd; five-node isolation checks passed. The lookup now avoids nested single quotes and passes a local nested-shell fixture; cluster-level unpack support remains pending. This is a CI harness issue and does not imply product runtime behavior. |
| In the isolated five-node kubeadm/Cilium fixture, CoreDNS's `loop` plugin detected forwarding to `127.0.0.1`, leaving CoreDNS CrashLooping and causing the source NetworkPolicy allow probe to time out. | `.github/scripts/nodemigrate-docker-preflight.sh` fixture | Feed kubelets an explicit resolver file containing only the runner's non-loopback upstream nameservers before Cilium/CoreDNS starts, then verify all CoreDNS replicas become Ready. | Confirmed by CoreDNS container logs in [run 36407648434](https://github.com/centerionware/not-k8s/actions/runs/36407648434), job `108880115025`. The fixture now selects `/run/systemd/resolve/resolv.conf` when available, rejects a loopback-only configuration, copies the usable nameserver entries to all nodes, and restarts kubelets with that `resolvConf`; migration rerun pending. |
| The migrated `CSINode` disappeared after nodemigrate read it but before the owner-reference patch, causing a 404 and aborting return migration. A CSI registrar delete/recreate race is the leading explanation, but the logs do not capture the actor. | `crates/nodemigrate/src/transfer.rs` | On a `CSINode` 404, re-read and recompute the owner-reference patch until the existing bounded registration deadline expires; retain resourceVersion conflict handling. | Confirmed by K3s job `108880115167` in [run 36407648434](https://github.com/centerionware/not-k8s/actions/runs/36407648434): the returned node reached Ready and the repair failed with `csinodes.storage.k8s.io ... not found`. The utility now retries this specific 404 under the 60-second deadline; focused migration rerun pending. |
| Traefik CRD integer schema maximums are changed during the upstream Kubernetes round trip, from `9223372036854775000` to `9223372036854776000`. | `nodemigrate` import/export JSON path or API-server serialization; exact mutation point not isolated | Trace each serialized hop and preserve the original CRD schema numeric value exactly; keep strict parity enabled and add a regression at the first lossy conversion boundary. | Confirmed as a real `f64` value difference by Kubernetes job `108880115214` in [run 36407648434](https://github.com/centerionware/not-k8s/actions/runs/36407648434), at return-stage strict parity. The existing nodeapiserver protobuf test does not yet localize the migration-path conversion. Unresolved; do not normalize or waive the mismatch. |

## Release target

The intended first nodemigrate release is coordinated with regular release
`v0.8.1`. Nodemigrate must not itself bump the shared version or create a
different regular release number. The component fixes above must be included
in the `v0.8.1` runtime and the standalone utility artifact must carry that
same version. This tracker does not authorize publication or merging.

See the [CI run record](NODEMIGRATE_CI_STATUS.md), [migration status](NODEMIGRATE_MIGRATION_STATUS.md), and [release status](NODEMIGRATE_RELEASE_STATUS.md).


## Follow-up: aggregated API discovery cache

The failure in [run 36300720876](https://github.com/centerionware/not-k8s/actions/runs/36300720876) remained after the fixture waited 60 seconds: `nodes.metrics.k8s.io` and `pods.metrics.k8s.io` were absent although the metrics APIService was reported `Available=True` and metrics-server added `metrics.k8s.io/v1beta1`. This was consistent with, but did not by itself prove, a stale APIService watch-cache snapshot in nodeapiserver.

The branch changed APIService route resolution and group discovery to read APIService objects directly from nodestore. Focused `nodeapiserver` quick-check passed at [run 36301964871](https://github.com/centerionware/not-k8s/actions/runs/36301964871), but the subsequent branch-runtime migration run [36301989587](https://github.com/centerionware/not-k8s/actions/runs/36301989587) still omitted both metrics resources after the APIService became `Available=True`; therefore stale reflector state was not the cause of this discovery failure. Source review found that the APIGroupDiscovery v2 builder deliberately emitted an empty `resources` list for APIService-backed groups. Since Kubernetes 1.37 clients can use that aggregated format, this is now the primary discovery defect being fixed: fetch each available backend's APIResourceList and include its resource/subresource entries in v2. That follow-up is not yet checked. The metrics-server-to-nodelet certificate trust failure remains separate and unresolved.


- **`nodeapiserver`: v2 aggregated discovery hides APIService resources.** The
  1.37 migration fixture kept omitting `nodes.metrics.k8s.io` and
  `pods.metrics.k8s.io` after APIService status became `Available=True` and
  metrics-server added `metrics.k8s.io/v1beta1`. The existing v2 builder returned
  the group-version with an empty `resources` array by design. It must fetch each
  available APIService's live `APIResourceList`, convert resource and subresource
  entries to `APIResourceDiscovery`, and exclude only backends whose discovery
  fetch actually fails. A pure regression now checks conversion; focused CI and a
  runtime retry are pending. Logs: `/tmp/nodemigrate-36301989587/`.

## 2026-09-29 migration workflow failures

- **Component:** `nodeapiserver` TokenRequest and projected ServiceAccount tokens.
  Run [36598031291](https://github.com/centerionware/not-k8s/actions/runs/36598031291)
  showed kubelet requests for live Pods failing because the handler required
  `spec.boundObjectRef.uid`. The handler now accepts an omitted UID, resolves
  the live Pod by namespace/name, and signs the actual current UID; an explicitly
  supplied stale UID is still rejected. A focused parser regression was added.
  Targeted `nodeapiserver` CI remains pending.
- **Component:** migration fixture's hostpath CSI redeployment. In the upstream
  lane of run `36598031291`, the fixture reapplied the upstream StatefulSet over
  the imported CSI workload before validation. The NodeStageVolume errors then
  reported that the imported volume IDs were absent from the driver's catalog.
  Forward migration now reuses the imported StatefulSet/catalog and only applies
  the nodelet staging mount. A migration rerun is pending.
- **Component:** `nodemigrate` Cilium join readiness. The five-node lane in run
  `36598031291` rolled back on cp-2 because its Cilium DaemonSet Pod had not yet
  been scheduled when host-state cleanup began. The utility now waits up to five
  minutes for that node's DaemonSet-owned agent before touching Cilium state.
  Targeted `nodemigrate` CI remains pending.
- **Component:** nodelet CRI sandbox reconciliation. The upstream lane also
  logged repeated `RunPodSandbox` failures because containerd still reserved
  CoreDNS sandbox names for older IDs after pod reconciliation timeouts. Stale
  sandbox replacement now stops/removes containers first, requires sandbox
  removal to succeed, and verifies the stale ID is gone before creating its
  replacement. Focused nodelet CI and migration validation are pending. Logs:
  `/tmp/nodemigrate-36598031291/`.

- **Component:** K3s migration-test fixture setup. In
  [run 36533241937](https://github.com/centerionware/not-k8s/actions/runs/36533241937),
  the K3s lane exited 22 before K3s was installed or a migration started. The
  fixture used a silent `curl -sfL` installer path, so diagnostics could not
  identify which URL failed. The worktree now fetches the K3s binary and
  SHA-256 file from the exact GitHub release with `gh`, verifies the binary,
  fetches the matching installer source through the GitHub API, and installs
  the preverified binary. The asset/checksum/installer retrieval passed an
  isolated check; workflow validation is pending.
- **Component:** `nodeapiserver` TokenRequest handling. In the same run, the
  upstream-to-nodestore lane passed source storage and legacy token/RBAC checks,
  then `kubectl create token migration-reader --duration=10m` received HTTP 400
  and failed to decode the response as JSON. The exact request validation error
  was not logged. The parser now treats `audiences: null` like an omitted
  audience list (the Kubernetes default), adds a regression for that wire form,
  and logs TokenRequest validation/issuance errors. The scoped nodeapiserver
  quick-check passed at SHA `188bbf736920211c53fa1244d1746b5c0e250a39`, but
  migration run [36536494864](https://github.com/centerionware/not-k8s/actions/runs/36536494864)
  reproduced the same HTTP 400 in both K3s and upstream lanes. Its audit entry
  confirms the TokenRequest path and status; no corresponding validation or
  issuance warning was captured. The failure detail was `JSON: expected value`,
  because the handler unconditionally passed request bytes to the JSON decoder
  even when kubectl negotiated protobuf. The handler now uses the declared
  format through `decode_virtual_request`, and the listener codec regression
  round-trips a protobuf TokenRequest and verifies the default audience and
  expiration. The first quick-check caught a test expectation that protobuf
  preserve an empty repeated `audiences` field; protobuf correctly omits it, so
  the fixture now exercises the default by leaving that field empty. A scoped
  regression and scoped quick-check [36539461032](https://github.com/centerionware/not-k8s/actions/runs/36539461032)
  passed at SHA `67cc4d3e`; the preceding run
  [36539012970](https://github.com/centerionware/not-k8s/actions/runs/36539012970)
  exposed only the test's incorrect empty-array expectation. The fix is now
  ready for another migration attempt. Logs: `/tmp/nodemigrate-36536494864/`.

- **Component:** Migration integration diagnostics. Run `36536494864` confirms
  both lanes fail with the same protobuf/JSON mismatch detail. The audit record
  captures the HTTP 400 and endpoint, and the client-visible Status detail
  identifies the decoder error. The request and response bodies are not
  otherwise retained; if the negotiated-codec fix does not resolve the next
  focused migration attempt, capture bounded wire diagnostics without logging
  issued credentials.

- **Component:** `nodemigrate` local Nodelet/CRI sandbox cleanup. All three
  branch-runtime lanes in [run 36749494255](https://github.com/centerionware/not-k8s/actions/runs/36749494255)
  restored Cilium's Socket LB links, then failed because Nodelet reconciled and
  started containers while `crictl rm` was removing containers from old
  sandboxes. The Kubernetes log captures the replacement container starting
  immediately before the failed removal; K3s and five-node logs report the same
  removal race. The worktree now pauses an active local Nodelet service only
  around this cleanup and starts it again on both success and failure without
  changing its enabled state. Quick-check `36753849376` caught missing result
  type annotations in two new failure-path tests; the annotations were fixed,
  and focused quick-check [36754232529](https://github.com/centerionware/not-k8s/actions/runs/36754232529)
  passed at SHA `7f7e7572`. This makes the fix ready for runtime validation in
  the migration matrix. The three original lanes rolled back and retained their
  protected exports; no general build or full e2e ran. Log:
  `/tmp/nodemigrate-36753849376-quick-check.log`.

## 2026-09-30 migration run 36754641851

- **Component:** `nodemigrate` CRI sandbox coordination with the active node
  agent. In K3s return migration, Cilium host-state repair completed, but K3s
  restarted container `85d623...` while nodemigrate was removing its sandbox;
  `crictl rm` failed because that container was running. The prior helper
  paused only a local Nodelet service, which is absent on the retained K3s
  source. In the upstream return lane, a Pod status likewise remained
  `ContainerStatusUnknown` during Cilium restart and the replacement endpoint
  appeared only after the fixture's readiness wait ended. The worktree now
  selects only an active K3s or upstream kubelet whose configured runtime
  endpoint matches the CRI endpoint being cleaned; when no upstream installation
  exposes that runtime, it falls back to the installed Nodelet service. For
  embedded K3s containerd, it pauses only the verified K3s main process and
  confirms CRI remains available before proceeding. A focused regression checks
  that unrelated K3s installations are not selected. The focused
  `nodemigrate,nodebootstrap` quick-check passed at `19ac3a5a` in
  [36763558588](https://github.com/centerionware/not-k8s/actions/runs/36763558588),
  and migration script validation passed at the same SHA in
  [36763555375](https://github.com/centerionware/not-k8s/actions/runs/36763555375).
  Live behavior is being exercised in matrix run
  [36764002800](https://github.com/centerionware/not-k8s/actions/runs/36764002800).
  Do not stop the K3s service before embedded-CRI cleanup. The
  K3s rollback restored nodestore but its final job check timed out; that check
  is downstream until distinguished from a separate readiness defect. Evidence:
  [run 36754641851](https://github.com/centerionware/not-k8s/actions/runs/36754641851),
  K3s job `110021665486`, Kubernetes job `110021665844`; logs in
  `/tmp/nodemigrate-36754641851-artifacts/`.
- **Component:** Kubernetes returned-stage Pod recovery after Cilium reset.
  `migration-standalone` was `ContainerStatusUnknown` at the five-minute wait
  deadline, while the Cilium agent was restarting; the Cilium log records its
  new endpoint shortly afterward. The current candidate serializes cleanup
  with the active kubelet only when its configured runtime matches the cleaned
  CRI endpoint. This behavior is pending quick-check and live migration
  verification; do not solve the readiness delay by lengthening the wait. Evidence:
  Kubernetes job `110021665844`, run
  [36754641851](https://github.com/centerionware/not-k8s/actions/runs/36754641851).
- **Component:** migration parity and fixture adaptation. Five-node migration
  reached the nodestore checkpoint and workload probes, but the strict source
  API comparison found 31 changed objects: 28 ClusterRoles, two PriorityClass
  descriptions, and `default/csi-hostpathplugin` StatefulSet. Its CSI
  StatefulSet spec was changed by the fixture to mount `/var/lib/nodelet`; this
  is a known test mutation that should be separated from migrated source state
  while retaining real CSI behavior. The 28 ClusterRole rule changes and two
  PriorityClass description changes are now traced to Nodebootstrap's control-
  plane join path: each joining node reapplied cluster-wide NodeApiserver
  bootstrap defaults over the API objects already imported from the source.
  Bootstrap seeding is now limited to initial cluster creation, while joins
  verify the existing policy and apply only the supplemental target-owned
  grants. The focused `nodebootstrap` regression passed in quick-check
  [36763558588](https://github.com/centerionware/not-k8s/actions/runs/36763558588).
  The snapshot normalizer now excludes only the exact fixture-added
  `nodemigrate-nodelet-root` volume and bidirectional mount, whose presence and
  host path remain directly asserted. All other StatefulSet fields and RBAC
  data remain strict. Strict five-node comparison and return behavior are being
  checked in matrix run [36764002800](https://github.com/centerionware/not-k8s/actions/runs/36764002800).
  Evidence: five-node job `110021665907`, run
  [36754641851](https://github.com/centerionware/not-k8s/actions/runs/36754641851);
  artifact log `/tmp/nodemigrate-36754641851-artifacts/nodemigrate-docker-preflight-36754641851/nodemigrate-docker-preflight.log`.
