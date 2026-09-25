# nodemigrate bug and fix tracker

Last updated: 2026-09-25

This living list tracks defects exposed while exercising nodemigrate against
the latest regular release (`v0.8.0` at the time of the run), their owning
components, branch fixes, and focused evidence. Release-backed failures caused
by old component binaries remain visible until the fixes are included in the
coordinated `v0.8.1` release.

## Confirmed bugs

| Bug | Owning component(s) | Fix in this branch | Focused evidence / state |
| --- | --- | --- | --- |
| Source objects can use an API version that the destination no longer serves even though discovery exposes another version of the same group and kind. ClusterTrustBundle import failed against `v0.8.0`. | `nodemigrate` | Negotiate the discovered destination version only for the same API group and kind; retain source version otherwise and report selected conversion. | Focused nodemigrate checks passed at SHA `954f1be3d12fb7f0334db8d8ff6efae28ca0e4250` ([run 36068373192](https://github.com/centerionware/not-k8s/actions/runs/36068373192)). Release integrations `36068417485` and `36071174265` confirmed ClusterTrustBundle/v1 was applied through v1beta1. |
| Protobuf encoding of Kubernetes `ExtraValue` arrays treated them as objects, so importing a CertificateSigningRequest returned HTTP 500. | `nodeapiserver` | Encode authentication, authorization, and certificates `ExtraValue` values as their repeated-string protobuf representation; regression tests cover encode/decode. | Quick-check for `nodeapiserver,nodecontroller,nodebootstrap` passed at fix SHA `f9e4b31139a30fb7a453161e4dc2295983fcef8f` ([run 36071174298](https://github.com/centerionware/not-k8s/actions/runs/36071174298)). `v0.8.0` reproduced the server error in run `36071174265`; fix must ship in `v0.8.1`. |
| Controllers shared `system:kube-controller-manager` for writes, which lacked permissions needed to create migration fixture pods; K3s-to-not-k8s stopped at CSI readiness with HTTP 403. | `nodecontroller`, `nodebootstrap` | Use a per-controller service-account identity for writes while retaining the base identity for shared informer reads; grant only the required impersonation and controller service-account RBAC. | Focused quick-check for both components passed at SHA `f9e4b31139a30fb7a453161e4dc2295983fcef8f` ([run 36071174298](https://github.com/centerionware/not-k8s/actions/runs/36071174298)). Latest-release run `36071174265` still reproduced 403s because it tests the unchanged `v0.8.0` runtime. |
| If target bootstrap/readiness/API import failed after source shutdown, nodemigrate left the source disabled and the target stack partially active, causing service/port conflict and no automatic recovery. The upstream `v0.8.0` CSR failure reproduced this path. | `nodemigrate` | Stop any partially installed nodestore services, restore the source's recorded service state, retain the protected export, and report both original failure and recovery outcome. Integration script checks restored source service/API and retained export after a failed migration. | Nodemigrate crate tests passed at SHA `f9e4b31139a30fb7a453161e4dc2295983fcef8f` ([run 36071161354](https://github.com/centerionware/not-k8s/actions/runs/36071161354)). Release-backed assertion passed in run `36071174265`, attempt 2: the original CSR import failed, kubelet and source API recovered, and the export remained present. |
| Five-node Docker preflight falsely rejected a healthy containerd CRI plugin because `ctr plugins ls` pads its fixed-width output with trailing spaces. | `.github/scripts/nodemigrate-docker-preflight.sh` (migration CI harness) | Match plugin type, ID, and final status as parsed fields instead of requiring unpadded end-of-line output. | Confirmed in five-node probe run `36074502550`: `io.containerd.grpc.v1 cri - ok` was present, but the previous end-anchored regex did not match. Field-based parser fix is in this branch; CI recheck pending. |

## Test infrastructure issue

| Issue | Owner | State |
| --- | --- | --- |
| Five privileged Docker node containers initially exited with code 255 before systemd readiness, preventing the required three-control-plane/two-worker isolation lane from running. | `.github/nodemigrate` image and Docker preflight | Keep each container's private cgroup namespace without shadowing it with the host cgroup bind mount; set systemd `/run` tmpfs mounts to mode 0755. Add specific checks for CRI, namespace, BTF, bpffs, and storage capabilities. | Systemd readiness passed in runs `36073431846` and `36074502550`. The latest run then exposed the separately tracked CRI output parser false-negative above. Five-node isolation is not yet verified. |

## Release target

The intended first nodemigrate release is coordinated with regular release
`v0.8.1`. Nodemigrate must not itself bump the shared version or create a
different regular release number. The component fixes above must be included
in the `v0.8.1` runtime and the standalone utility artifact must carry that
same version. This tracker does not authorize publication or merging.

See the [CI run record](NODEMIGRATE_CI_STATUS.md), [migration status](NODEMIGRATE_MIGRATION_STATUS.md), and [release status](NODEMIGRATE_RELEASE_STATUS.md).
