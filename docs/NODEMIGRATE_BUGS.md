# nodemigrate bug and fix tracker

Last updated: 2026-09-24

This living list tracks defects exposed while exercising nodemigrate against
the latest regular release (`v0.8.0` at the time of the run), their owning
components, branch fixes, and focused evidence. Release-backed failures caused
by old component binaries remain visible until the fixes are included in the
coordinated `v0.8.1` release.

## Confirmed bugs

| Bug | Owning component(s) | Fix in this branch | Focused evidence / state |
| --- | --- | --- | --- |
| Source objects can use an API version that the destination no longer serves even though discovery exposes another version of the same group and kind. ClusterTrustBundle import failed against `v0.8.0`. | `nodemigrate` | Negotiate the discovered destination version only for the same API group and kind; retain source version otherwise and report selected conversion. | Focused nodemigrate checks passed at SHA `954f1be3d12fb7f0334db8d8ff6efae28ca0e4250` ([run 36068373192](https://github.com/centerionware/not-k8s/actions/runs/36068373192)). Release integration confirmed ClusterTrustBundle/v1 was applied through v1beta1; full run still failed later on CSR. |
| Protobuf encoding of Kubernetes `ExtraValue` arrays treated them as objects, so importing a CertificateSigningRequest returned HTTP 500. | `nodeapiserver` | Encode authentication, authorization, and certificates `ExtraValue` values as their repeated-string protobuf representation; regression tests cover encode/decode. | Quick-check passed for `nodeapiserver,nodemigrate` at SHA `336ea350502288d55bb6a57763494fa0aa89a475` ([run 36065050713](https://github.com/centerionware/not-k8s/actions/runs/36065050713)). `v0.8.0` still has the defect; focused current-branch check must be rerun at the final fix SHA. |
| Controllers shared `system:kube-controller-manager` for writes, which lacked permissions needed to create migration fixture pods; K3s-to-not-k8s stopped at CSI readiness with HTTP 403. | `nodecontroller`, `nodebootstrap` | Use a per-controller service-account identity for writes while retaining the base identity for shared informer reads; grant only the required impersonation and controller service-account RBAC. | Fix is present in this branch. The `v0.8.0` integration failure is recorded in [run 36068417485](https://github.com/centerionware/not-k8s/actions/runs/36068417485). Focused quick-check for both owning components is pending. |
| If target bootstrap/readiness/API import failed after source shutdown, nodemigrate left the source disabled and the target stack partially active, causing service/port conflict and no automatic recovery. The upstream `v0.8.0` CSR failure reproduced this path. | `nodemigrate` | Stop any partially installed nodestore services, restore the source's recorded service state, retain the protected export, and report both original failure and recovery outcome. Integration script now checks restored source service/API and retained export on failed migration. | Runtime regression is added; shell validation and targeted nodemigrate checks are pending. The release-backed lane will remain red for v0.8.0's CSR codec, but should report rollback assertions. |

## Test infrastructure issue

| Issue | Owner | State |
| --- | --- | --- |
| Five privileged Docker node containers exit with code 255 before systemd readiness, so the required three-control-plane/two-worker isolation and migration lane cannot run. This is currently a test-host/isolation limitation, not a confirmed nodemigrate component defect. | `.github/nodemigrate` image and Docker preflight | Image builds, but preflight remains failed in [run 36068417485](https://github.com/centerionware/not-k8s/actions/runs/36068417485). Investigate Docker/systemd startup or use a runner with verified virtualization; no five-node parity claim is allowed. |

## Release target

The intended first nodemigrate release is coordinated with regular release
`v0.8.1`. Nodemigrate must not itself bump the shared version or create a
different regular release number. The component fixes above must be included
in the `v0.8.1` runtime and the standalone utility artifact must carry that
same version. This tracker does not authorize publication or merging.

See the [CI run record](NODEMIGRATE_CI_STATUS.md), [migration status](NODEMIGRATE_MIGRATION_STATUS.md), and [release status](NODEMIGRATE_RELEASE_STATUS.md).
