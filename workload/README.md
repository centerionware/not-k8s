# not-k8s workload profiling results

Measured **nodelet** vs a **real, standalone upstream kubelet** across a
full spin-up / idle-with-workload / spin-down cycle of a fixed synthetic
workload, on the same stripped `k3s server --disable-agent` control plane
and the same containerd — only the node agent differs. Sibling to
[../README.md](../README.md) (idle-only steady-state comparison).

## Latest result

**[latest/README.md](latest/README.md)** — commit `76b8b6e58b6c`, [workflow run](https://github.com/centerionware/not-k8s/actions/runs/32423795548).

## Methodology

- `.github/workflows/profiling.yml`, dispatched with `profile_mode: workload`.
  Same 6-runner-in-parallel, 3-replicates-per-agent, real-standalone-installer
  setup as idle mode (see ../README.md) — only what's measured after bootstrap
  differs.
- **Spin up**: `deploy/lib/profiling-workload.sh up` creates N namespaces
  (default 10), one pod each, and waits for every pod's `Ready` condition —
  timed with a precise wall-clock stopwatch, not sampling-loop granularity.
  `deploy/measure.sh` samples RSS/CPU concurrently, stopping the instant the
  last pod goes Ready rather than running a fixed window.
- **Idle with workload present**: the same fixed-window `deploy/measure.sh`
  sample idle mode uses, just with the N pods now running.
- **Spin down**: mirrors spin-up — all N namespaces deleted, timed until
  every one is actually gone, sampled concurrently the same way.
- A final job downloads all runners' data, aggregates the 3 replicates per
  agent (mean, min–max range) per phase, and renders the charts + report.

Every run's full output is kept at `history/<YYYY-MM-DD_HH-MM-SS>/`;
`latest/` mirrors the most recent one.

## Run this yourself

```
sudo bash deploy/lib/profiling-workload.sh up   10 /tmp/spinup   nodelet
sudo ./deploy/measure.sh 120 /tmp/idle nodelet
sudo bash deploy/lib/profiling-workload.sh down 10 /tmp/teardown nodelet
```

Or dispatch `gh workflow run profiling.yml -f profile_mode=workload`.
