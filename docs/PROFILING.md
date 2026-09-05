# Full-stack load profiling

The optional `stack` mode of `profiling.yml` builds, boots, measures, renders,
and publishes on one disposable Ubuntu runner. There are no intermediate
Actions artifacts. The existing `comparison` mode remains separate; its legacy
nodelet/kubelet methodology is not a full-stack performance comparison.

## Run a branch or release tag

After the workflow changes are available on the selected workflow ref:

```sh
gh workflow run profiling.yml --ref perf/stack-load-profiling \
  -f mode=stack -f source_ref=nodeapiserver \
  -f build_profile=profiling -f sample_seconds=120
```

Use an existing branch, tag, or SHA for `source_ref`; empty means the workflow's
own commit. The tooling and target source are checked out separately and both
SHAs are recorded. Once merged, select `--ref main` for tooling. The target must
support the combined `notk8s` applets and Cargo `profiling` profile.

`profiling` uses release optimization with symbols and forced frame pointers.
`debug` is supported for diagnosis but **not** for release CPU comparisons.
Ordinary release artifacts remain stripped and unchanged. This rebuilds tagged
source with instrumentation; it does not measure the exact stripped release
asset byte-for-byte. Record that distinction in performance claims.

The release workflow defaults `profile_after_release` to true. It calls this
same single-runner job after successful publication, using the published tag.
Set it false to skip the extra cost. This does not depend on a `release` event
trigger (events created using `GITHUB_TOKEN` ordinarily do not start workflows).
A profiling failure after publication does not undo the published release.

## What is measured

- The short-lived `nodebootstrap` applet and its child commands during setup,
  separately from steady-state daemons.
- Six distinct runtime PIDs: nodestore, nodeapiserver, nodescheduler,
  nodecontroller, nodelet, and nodeproxy, simultaneously in each phase.
- Idle with a small ready HTTP Deployment, then a bounded loaded window:
  two API CRUD workers, one ConfigMap watch, periodic Deployment scaling, and
  approximately five HTTP requests/second from a Pod through a ClusterIP.
- Per-process one-second CPU counters, RSS and PSS, plus 49 Hz software
  `cpu-clock` perf samples. CPU percentage is relative to one logical CPU.
- Workload errors, latency per kubectl operation, traffic successes, image IDs,
  kernel/hardware/compiler versions, source SHA, and executable hash.

This samples API/storage/controller/container/network paths; it does not cover
CSI/DRA, multi-node networking, HA, large-cluster scale, or every admission path.
It is not Kubernetes conformance, a maximum-throughput benchmark, or a claim of
performance dominance. kubectl process startup contributes to operation latency.
The generator and perf consume host CPU; repeated equivalent runs are necessary
before comparing changes. Containerd/flannel/workload CPU is outside the six
Rust daemon series, so their sum is **not total cluster CPU/RSS**.

The Python runner accepts `--replicas=1..10`, `--workers=1..4`, and
`--seconds=30..600`. Namespace cleanup is attempted on failure. A component
restart during a capture invalidates the run rather than silently switching PID.
Perf failure is explicit: the stack mode never substitutes strace or sleeping
for flame graphs. Zero sampled stacks are recorded as `no-samples.txt`, not zero
CPU. Rendering happens after capture to keep symbolization out of the workload.

## Results and storage

Results go to the existing `profiling-results` branch under
`history/<timestamp>-<run>-<attempt>-stack/`; `latest-stack.md` links to the latest
stack result without duplicating its archive. Legacy `latest/` is preserved.
SVGs and time-series CSVs are browseable. The complete bundle contains raw
`perf.data`, decoded/folded stacks, summaries, controlled-workload diagnostics,
and the matching symbolized executable under `symfs/` at its original paths.
Hardlinks avoid storing identical installed/build executables twice in the tar.

The gzip archive is split into 48 MiB parts, each with a SHA-256 checksum.
The default compressed budget is 512 MiB (`archive_limit_mib`, 64..2048).
Oversized results fail publication; data is never silently truncated. These
files grow Git history, even though no Actions artifact storage is used. Review
storage growth before lengthening windows or enabling frequent branch runs.

Download all parts plus `SHA256SUMS` into a fresh directory:

```sh
sha256sum -c SHA256SUMS
cat profile.tar.gz.part-* | tar -xz
perf report --symfs "$PWD/symfs" -i load/nodeapiserver/perf.data
```

System-library symbols still depend on matching libraries/debug packages; the
bundle includes the Rust executable, not an entire runner filesystem. Do not
profile production or secret-bearing workloads and publish their diagnostics to
a public branch. This mode creates only disposable, fixed-content test objects.

## CPU/memory comparisons without flamegraphs

Choose `components` for one component or a selected combination, and
`whole-stack` for distribution-daemon totals. Both use the same idle/load
fixture as `stack`, but no perf collection. not-k8s is built with its normal
optimized, stripped release profile on its measurement runner (no build artifact
handoff). Upstream is a real kubeadm Kubernetes cluster, not our bootstrap's
upstream-apiserver-only option. Kubernetes 1.34.3 and k3s v1.34.3+k3s1 are pinned;
actual server versions and target source identity are recorded with the results.

```sh
# One component; also accepts nodeapiserver,nodestore or all.
gh workflow run profiling.yml --ref perf/stack-load-profiling \
  -f mode=components -f source_ref=nodeapiserver \
  -f components=nodeapiserver -f baseline=k8s -f sample_seconds=120

# Whole distribution vs BOTH upstream choices in one run.
gh workflow run profiling.yml --ref perf/stack-load-profiling \
  -f mode=whole-stack -f source_ref=nodeapiserver \
  -f baseline=both -f sample_seconds=120
```

`baseline=k8s`, `k3s`, and `both` are selectable for whole-stack. k3s runs its
components inside one process: per-component CPU/RSS cannot be honestly isolated,
so component mode rejects k3s. Graphs map canonical names to upstream equivalents:
nodestore/etcd, nodeapiserver/kube-apiserver, nodescheduler/kube-scheduler,
nodecontroller/kube-controller-manager, nodelet/kubelet, nodeproxy/kube-proxy.

Every successful comparison publishes a README with embedded **per-component
and combined** CPU/RSS/PSS PNG graphs, CSVs, workload diagnostics and metadata at
`profiling-results/comparisons/<run>-<attempt>/`. `latest-comparison.md` links there.
Whole-stack adds containerd, Flannel and CoreDNS (Flannel is embedded in k3s).
It excludes workload processes, shims and unrelated host services; it is not a
whole-machine measurement. Shared pages may be counted repeatedly in summed RSS;
PSS apportions them. Missing components fail capture instead of becoming zeros.

Each stack runs once on its own fresh hosted runner. Hardware/noisy-neighbor
variation and dependency versions matter; repeat before making performance claims.
Component comparisons measure components within their respective full stacks,
not causal one-component swaps with identical surrounding implementations.
Metrics publish directly to the results branch; failed legs do not produce a
misleading combined report. No flamegraphs or large binary archives are collected
by these comparison modes. Use `stack` for diagnostic SVG flamegraphs.

## Before interpreting compatibility or optimizing

Keep the final unfiltered e2e gate. Next, run version-matched upstream conformance
and differential API tests, exercise existing Helm/controller workloads, and
cover multi-node/restart/persistence behavior. In particular, audit documented
GC foreground/orphan semantics and cluster-scoped ownership gaps; background
Deployment deletion passing does not establish complete GC compatibility.
Use these profiles to choose the next optimization, not as permission to remove
recovery paths, validation, or supported behavior.
