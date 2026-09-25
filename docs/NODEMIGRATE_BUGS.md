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
| Five-node Docker preflight falsely rejected a healthy containerd CRI plugin because `ctr plugins ls` pads its fixed-width output with trailing spaces. | `.github/scripts/nodemigrate-docker-preflight.sh` (migration CI harness) | Match plugin type, ID, and final status as parsed fields instead of requiring unpadded end-of-line output. | Confirmed in run `36074502550`; fixed parser passed in `36075750885`, which proceeded to the BPF compile probe. |
| The Docker BPF probe did not compile because its global symbol was named `license`, which clang's BPF assembler rejected as a duplicate symbol. | `.github/nodemigrate/bpf-probe.c` (migration CI harness) | Use the conventional `LICENSE` symbol name while retaining the required ELF `license` section. | Confirmed by the clang diagnostics in run `36075750885`; rename is in this branch and awaits a CI preflight rerun. |
| The five-node image resolved Ubuntu's `bpftool` wrapper, which searched for tools matching the runner's Azure kernel version and failed because that versioned binary was absent in the container. | `.github/nodemigrate/five-node.Dockerfile` (migration CI harness) | Install the generic versioned Linux tools package and point `/usr/local/bin/bpftool` directly at the packaged executable, avoiding the host-kernel-version wrapper lookup. | Confirmed in Docker-only run `36077006621`; the probe reached bpftool, then the wrapper reported no tool for kernel `6.17.0-1022-azure`. The first fix attempt assumed the binary was nested under `/usr/lib/linux-tools`; the image log showed the package install succeeded but that search path did not. The search now covers Ubuntu's versioned `/usr/lib/linux-tools-*` package directory. |

## Test infrastructure issue

| Issue | Owner | State |
| --- | --- | --- |
| Five privileged Docker node containers initially exited with code 255 before systemd readiness, preventing the required three-control-plane/two-worker isolation lane from running. | `.github/nodemigrate` image and Docker preflight | Keep each container's private cgroup namespace without shadowing it with the host cgroup bind mount; set systemd `/run` tmpfs mounts to mode 0755. Add specific checks for CRI, namespace, BTF, bpffs, and storage capabilities. | Systemd readiness passed in runs `36073431846`, `36074502550`, `36075750885`, and `36077006621`. The CRI parser and BPF source symbol issues are fixed; the latest run reached the BPF loader and exposed the packaged `bpftool` wrapper issue above. Five-node isolation is not yet verified. |

## Release target

The intended first nodemigrate release is coordinated with regular release
`v0.8.1`. Nodemigrate must not itself bump the shared version or create a
different regular release number. The component fixes above must be included
in the `v0.8.1` runtime and the standalone utility artifact must carry that
same version. This tracker does not authorize publication or merging.

See the [CI run record](NODEMIGRATE_CI_STATUS.md), [migration status](NODEMIGRATE_MIGRATION_STATUS.md), and [release status](NODEMIGRATE_RELEASE_STATUS.md).
