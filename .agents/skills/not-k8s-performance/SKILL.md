---
name: not-k8s-performance
description: Measure and improve not-k8s CPU and memory cost with reproducible baselines, component attribution, and Kubernetes correctness checks. Use for profiling, efficiency regressions, or performance gates during a version upgrade; not for unmeasured optimization during ordinary bug fixes.
---

# Preserve measured CPU and memory efficiency

Read [AGENTS.md](../../../AGENTS.md). Determine whether the request is to
measure, diagnose, optimize, or validate an upgrade. Profiling an authorized
test deployment does not authorize replacing a production stack. Preserve
current compatibility and the user's baseline/measurement scope.

## Define a comparable experiment

Record candidate and baseline SHAs/artifact digests, Kubernetes target, build
profile/features, architecture, host/kernel/cgroup settings, runtime/CNI/drivers,
component set, workload, object counts, warm-up, duration, and repetitions.
Use optimized comparable binaries for performance claims; symbolized/debug
binaries can locate hotspots but do not establish release CPU ratios.
Build in CI, not on the constrained development host.

Measure both the previous not-k8s baseline (regression detection) and the
equivalent upstream/k3s stack when claiming an advantage over Go. Keep the
same enabled behavior and workload. Separate idle, steady traffic, controller
churn, burst/startup, watch reconnect, and recovery costs. A quiet cluster with
missing controllers is not an efficient equivalent implementation.

Define the measurement boundary. Report component CPU-seconds and RSS/PSS
where available, plus a clearly scoped whole-stack total. Explain shared-memory
accounting and which external processes are included. Do not compare one Rust
process against all of k3s or double-count a combined-binary process. Record
actual PIDs/executables; a loose command-line match can select a client or the
wrong process.

Use fixed-duration CPU counters or CPU per completed operation, not a single
`top` percentage. Report latency/convergence, throughput, error/retry rates,
and peak as well as steady memory, so reduced CPU cannot hide delayed work or
data loss. Use repeated samples and report spread/noise; do not select only the
best run. Agree on budgets when needed; do not invent a claimed requirement
such as "always 10x faster" from the project's ambition.

## Use the existing tooling accurately

Read `deploy/profile-process.sh` before using it. It supports an exact PID,
bounded duration, output directory, and optional journal unit; it captures
`perf.data`, a flame graph, thread snapshots, and executable identity without
restarting the process. Use a verified disk-backed output path on small hosts.
Inspect its prerequisite/install behavior and available permissions first.

Read `deploy/measure.sh` for its actual component accounting and output schema.
Inspect `.github/workflows/profiling.yml` before dispatch: its historical
nodelet-vs-kubelet comparison is not a full-stack apiserver benchmark. Reuse
applicable tooling; adapt mismatched setup in a scoped change rather than
presenting its output as evidence for a different experiment.

## Optimize an observed cost

Attribute the cost to useful work versus duplicate work, allocations/decoding,
contention, wakeups/polls, redundant lists/watches, unbounded retries, or I/O.
Read the hot path and its lifetime/ownership before changing it. Cache immutable
schema indexes once, share informers where appropriate, coalesce keyed work,
and bound retries when evidence points there. Do not assume all caching helps:
account for invalidation, stale data, memory growth, and recovery.

Change the demonstrated bottleneck and rerun the same experiment. Preserve
raw artifacts and compare equivalent windows/configurations. Verify affected
behavior through [the CI skill](../not-k8s-ci/SKILL.md); the full compatibility
gate still applies. Do not disable admission, drop events, shorten watch
history without testing relist cost, omit features, or suppress retries needed
for convergence to improve a chart.

Finish with measured deltas, variance, tradeoffs, correctness evidence, and
limits. A flame graph identifies where CPU went; it does not alone quantify
the end-to-end improvement. Keep "faster in this experiment" separate from
universal CPU/memory dominance.
