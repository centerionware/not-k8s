# nodemigrate implementation and integration status

Last updated: 2026-09-24

This is the living implementation status record for the full scope in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). Capability marks describe code
that exists; verification marks describe evidence from a run. A passing
compile or unit test does not mark a real migration path as verified.

## Migration coverage

| Scenario or behavior | Implementation | Verification |
| --- | --- | --- |
| Detect local K3s service, Kine/etcd mode, config, and network settings | Implemented | Unit tests only; targeted CI rerun pending after compiler fixes |
| Detect K3s agent and upstream Kubernetes worker roles | Implemented in inventory, including role-specific services and external CNI detection | Focused role fixtures passed in [run 35959159615](https://github.com/centerionware/not-k8s/actions/runs/35959159615). |
| Replace a worker against an existing nodestore cluster | Worker path avoids cluster-wide object import, requires a joined destination, explicitly guards same-name Node removal, waits for fresh Ready registration, preserves labels/taints/unschedulable state, and snapshots local hostPath/local PV data from the destination PV inventory | Existing role and affinity checks passed in [run 35960475945](https://github.com/centerionware/not-k8s/actions/runs/35960475945). Scheduling-state preservation is pending focused CI. Real worker cutover, data restoration, rollback, and uninstall behavior remain unverified. |
| Replace a migrating control-plane Node object | Forward and reverse migration require `NODEMIGRATE_REPLACE_NODE=true` when the destination already has the local node name. The stale Node is deleted before waiting for fresh Ready, then its labels, taints, and unschedulable setting are restored; when no destination Node exists, these scheduling fields come from the source Node. A staged reverse cutover returns before API readiness, so its coordinator must verify fresh registration after quorum returns. | Destination replacement validation passed in [run 35966523279](https://github.com/centerionware/not-k8s/actions/runs/35966523279); source/destination scheduling-state preservation added afterward and targeted crate CI pending. No live registration or replacement-member runtime evidence. |
| Return a not-k8s worker to a retained K3s/upstream worker | Worker inventory recognizes nodelet-only hosts; reverse path snapshots local PV data, stops nodebootstrap worker services, starts the retained target agent, optionally replaces a stale same-name Node, and waits for fresh Ready without re-importing cluster resources | Role fixtures and nodemigrate checks passed in [run 35960872299](https://github.com/centerionware/not-k8s/actions/runs/35960872299). No live reverse worker cutover or multi-node round trip recorded. |
| Stop upstream control-plane static pods before cutover | After kubelet stops, remove file-managed kube-system pod sandboxes via CRI while retaining source manifests; endpoint is detected from kubelet instance config or overridden with `NODEMIGRATE_CRI_ENDPOINT` | CRI inventory and endpoint fixtures passed in [run 35960125413](https://github.com/centerionware/not-k8s/actions/runs/35960125413). No live kubeadm cutover evidence. |
| Detect an upstream control plane and read CIDRs and cluster domain | Implemented | Focused fixture passes after the rooted manifest path correction |
| Identify Cilium and other known CNIs from the active host CNI configuration | Implemented for recognized CNI config names and plugin types | Focused fixture tests passed in [run 35955823452](https://github.com/centerionware/not-k8s/actions/runs/35955823452) |
| Export all discovered API resource pages, including CRDs and add-ons | Implemented | Not yet exercised against a real cluster |
| Carry static hostPath/local PV data and preserve CSI-backed data on the same host | Uses local Node labels to filter hostPath/local PV backups by required node affinity when available; if labels cannot be read, logs a warning and backs up every safe path found locally rather than omitting unknown candidates. CSI data remains with its storage provider. | Affinity fixtures passed in [run 35965492161](https://github.com/centerionware/not-k8s/actions/runs/35965492161); no round trip evidence |
| K3s → nodestore with Flannel | Implemented | No real migration run recorded |
| K3s → nodestore with external CNI such as Cilium | Implemented as external-CNI bootstrap and API-resource transfer | No real migration run recorded |
| Upstream Kubernetes → nodestore | Implemented for detected local control planes | No real migration run recorded |
| Join an existing nodestore control plane using nodebootstrap environment settings | Implemented through nodebootstrap join configuration, then a worker bootstrap registers the replacement node | Focused command tests passed in [run 35955823452](https://github.com/centerionware/not-k8s/actions/runs/35955823452); no real replacement-node run recorded |
| Migrate later source control planes without re-importing cluster-wide objects | `skip-api-import=true` still creates a per-node protected export and local-volume snapshot, then joins the existing nodestore cluster without applying the cluster export; request validation rejects reverse/worker use and runtime validation requires an existing-cluster join | Focused CI pending for current change; first-control-plane import and multi-node runtime ordering remain unverified |
| Return a multi-control-plane cluster when either datastore needs peers to reach quorum | `stage-target=true` exports nodestore state, disables the local nodestore stack, starts the retained target control plane, and returns before API readiness; later CP can import after target quorum forms. `skip-api-export=true` supports the final CP after import, requiring a ready destination API and backing up local PV data from that API. Both options require CP roles at both ends. | Request and role validation passed in [run 35965492161](https://github.com/centerionware/not-k8s/actions/runs/35965492161); no three-CP runtime evidence. This is an operator-ordered protocol, not an automatic cluster coordinator. |
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

The workflow is manual. Do not run it unless the user authorizes
real-cluster/e2e execution. The standard e2e and build gates remain excluded
by the user's current instruction.

## Merge-gate status

The required pre-merge runtime evidence is still missing for both gates:
single-node K3s+Cilium → not-k8s → K3s with no differences in the canonical
checked state, and upstream Kubernetes with three control-plane nodes plus two
workers → not-k8s → upstream Kubernetes. Both require successful workload,
add-on, ingress, certificate, persistent-data, and Cilium checks at every
stage; the five-node case also requires the existing-cluster join/replacement
path. The existing single-host lane compares canonical initial and returned
snapshots, including node names/roles/readiness, workload/add-on/ingress specs,
PV/PVC bindings, Cilium and required CRD state, and certificate-secret content
by digest. It does not provision or verify five distinct isolated nodes. QEMU
or another suitable isolation method can host those nodes on one CI node;
Docker is acceptable only if the environment fully simulates the behaviors
under test. No runtime gate has been dispatched. The new skip-import path
addresses repeated cluster-wide API application on later forward control-plane
joins. The staged reverse path still needs an orchestrator to sequence quorum
recovery and verify freshness for nodes whose staged invocation returns before
the target API is ready.

## Verification history

| Date | SHA | Check | Result | Evidence |
| --- | --- | --- | --- | --- |
| 2026-09-24 | `daac9ad05285e6ff749d4c51a18f9b71c8fb7dee` | Quick-check `nodebootstrap,nodemigrate` | Passed before bidirectional changes; does not validate reverse migration | [Run 35949477611](https://github.com/centerionware/not-k8s/actions/runs/35949477611) |
| 2026-09-24 | `55704b1c202c248ab633aac8c74877c46499e59f` | nodemigrate crate checks and targeted quick-check | Failed to compile; fixes are in the current worktree and follow-up runs are pending | [Crate run 35952766075](https://github.com/centerionware/not-k8s/actions/runs/35952766075), [quick-check run 35952782893](https://github.com/centerionware/not-k8s/actions/runs/35952782893) |
| 2026-09-24 | `4e932917080e21412c79625fb2b79d08f5e62c0f` | nodemigrate crate tests | Compiled; 9 passed, upstream network detection fixture failed due to doubled-root manifest path; correction pending | [Run 35954374767](https://github.com/centerionware/not-k8s/actions/runs/35954374767) |
| 2026-09-24 | `4ef3585eaea6beae51ffd1116134435b0edc305e` | nodemigrate crate tests | Passed after correcting rooted manifest path | [Run 35954606805](https://github.com/centerionware/not-k8s/actions/runs/35954606805) |
| 2026-09-24 | `0fe454dad5b3fb194b72a48bc657ed0512a7f29a` | Nodemigrate crate tests | Passed; covers skip-import parsing and rejection unless a control-plane joins an existing nodestore cluster. Does not verify a live multi-control-plane migration. | [Run 35962922792](https://github.com/centerionware/not-k8s/actions/runs/35962922792) |
| 2026-09-24 | `0fe454dad5b3fb194b72a48bc657ed0512a7f29a` | PR shell validation and commit convention | Passed | [Shell run 35962922860](https://github.com/centerionware/not-k8s/actions/runs/35962922860), [commit run 35962919894](https://github.com/centerionware/not-k8s/actions/runs/35962919894) |
| — | — | K3s integration round trip | Not run | Manual workflow above |
| — | — | Upstream Kubernetes integration round trip | Not run | Manual workflow above |

Update this table after each implementation or verification change. Record the
exact SHA, workflow run, source distribution, CNI, and whether each stage
passed, failed, or was skipped.

The integration workflow currently defaults to K3s `v1.35.0+k3s1`, Cilium
`1.20.2`, cert-manager `v1.21.2`, and Helm `v3.17.3`. The kubeadm lane follows
the Kubernetes stable minor at dispatch time. Override the pinned values with
the workflow environment when testing a different compatibility combination,
and record the resolved versions in the run log.
