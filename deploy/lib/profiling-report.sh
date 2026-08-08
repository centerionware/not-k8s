#!/usr/bin/env bash
# profiling-report.sh — render a nodelet-vs-real-upstream-kubelet idle
# resource-footprint comparison as GitHub-flavored markdown, embedding the
# real PNG charts render-profiling-charts.py already built from measure.sh's
# per-second CSV time series (not just a single final-average number).
#
# Two modes:
#   profiling-report.sh <nodelet_dir> <kubelet_dir> <commit_sha> <run_url>
#       Renders one run's README.md (the main file in its
#       history/<sha>-<stamp>/ folder — see profiling.yml's publish step).
#       Assumes rss-over-time.png/cpu-over-time.png already exist alongside
#       wherever this output gets written (render-profiling-charts.py's job,
#       not this script's).
#   profiling-report.sh --readme <commit_sha> <run_url> <run_stamp>
#       Renders the profiling-results branch's top-level README.md,
#       documenting the methodology and linking to the latest run's real
#       files + the workflow run that produced them.
#
# Pure text in, text out — testable without a real runner.
set -euo pipefail

# ── --readme mode (profiling-results branch's top-level README.md) ──────────

if [[ "${1:-}" == "--readme" ]]; then
    COMMIT_SHA="${2:?usage: profiling-report.sh --readme <commit_sha> <run_url> <run_stamp>}"
    RUN_URL="${3:?}"
    RUN_STAMP="${4:?}"
    SHORT_SHA="${COMMIT_SHA:0:12}"

    cat <<EOF
# not-k8s profiling results

Real, measured idle resource footprint for **nodelet** vs a **real,
standalone upstream kubelet** — same stripped \`k3s server --disable-agent\`
control plane, same containerd, only the node agent itself different
between the two measurement runs.

## Why a real upstream kubelet, not a default k3s install

A default \`k3s server\` (no \`--disable-agent\`) runs kubelet as an embedded
goroutine inside the *same OS process* as the apiserver/etcd/controller-
manager/scheduler — there's no separate process to isolate "kubelet's own
number" from at all. This project instead downloads and runs a real,
version-matched, standalone \`kubelet\` binary
(\`deploy/lib/upstream-kubelet.sh\`) against the exact same control plane and
containerd nodelet already uses, giving kubelet the identical footing
nodelet is measured on. See that script's own header comment for exactly
what it does (and doesn't) set up — it's a measurement rig, not a
production posture (no TLS bootstrap, no kube-proxy/CNI, since no pods are
actually scheduled in either measurement).

## Why six separate runners, not one runner measuring both agents in sequence

Measuring nodelet, then swapping in kubelet on the *same* runner risks
exactly the kind of bias this report exists to rule out — disk cache
state, memory fragmentation, or anything else accumulated by whichever
phase ran first quietly advantaging or disadvantaging whichever ran
second. \`.github/workflows/profiling.yml\` instead runs a matrix of 6
independent GitHub Actions runners (3 replicates × 2 agents), provisioned
identically (the same \`bootstrap-source.sh --with-cri\` call on all of
them — the kubelet legs just stop nodelet and swap in kubelet afterward,
never measuring nodelet at all) and measured *in parallel*, so the only
difference between any two runners by the time measurement starts is
which node agent is under test. 3 replicates per agent (not 1) so one
noisy run can't skew the conclusion — the report shows a real mean and
min–max range across replicates.

## Latest result

**[latest/README.md](latest/README.md)** — commit \`$SHORT_SHA\`, produced by
[this workflow run]($RUN_URL).

![RSS over time](latest/rss-over-time.png)
![CPU %% over time](latest/cpu-over-time.png)

Raw per-second time series (1 sample/sec — the full-resolution data the
charts above are rendered from), 3 replicates per agent:
- \`latest/raw-data/nodelet-1\`/\`nodelet-2\`/\`nodelet-3\`-timeseries.csv
- \`latest/raw-data/kubelet-1\`/\`kubelet-2\`/\`kubelet-3\`-timeseries.csv
- \`latest/raw-data/<agent>-<n>-k3s-server-timeseries.csv\` — the control plane's own
  footprint on that same runner, for cross-checking it measured the same
  regardless of which node agent it was paired with
- \`latest/raw-data/<agent>-<n>-summary.txt\` — the full human-readable +
  machine-readable (\`MEASURE_*\`) output \`deploy/measure.sh\` produced on
  that runner

## Methodology

1. 6 GitHub Actions runners (a \`strategy.matrix\` of \`agent: [nodelet,
   kubelet]\` × \`replicate: [1, 2, 3]\` in \`.github/workflows/profiling.yml\`)
   are provisioned identically and in parallel: each runs
   \`deploy/bootstrap-source.sh --with-cri\`, which builds nodelet from
   source and installs the stripped k3s control plane + containerd +
   nodelet as a systemd service.
2. On the 3 kubelet-leg runners only: \`nodelet.service\` is stopped and
   \`deploy/lib/upstream-kubelet.sh start\` installs and starts a real
   kubelet binary — version-matched to that runner's own k3s-embedded
   Kubernetes release — against the same control plane and containerd.
   The 3 nodelet-leg runners are untouched.
3. Every runner settles for 30s, then \`deploy/measure.sh \$SAMPLE_SECS\`
   samples RSS and CPU **every second** for the sample window, writing a
   real per-second CSV time series for both the node agent and the
   control plane — not just a single before/after delta. Where the
   runner's \`perf_event\` access allows it (often restricted on
   virtualized cloud runners, since the hypervisor may not expose the PMU
   to the guest at all), it also captures real hardware
   cycles/instructions for the whole window via \`perf stat\`.
4. Every runner uploads its measurement directory as a build artifact; a
   final job downloads all 6, aggregates the 3 replicates per agent
   (mean, min–max range) and renders real PNG charts from the CSVs
   (\`deploy/lib/render-profiling-charts.py\`, matplotlib + numpy), and
   \`deploy/lib/profiling-report.sh\` wraps that into the markdown report
   published here.

Every run's full output is kept under \`history/<YYYY-MM-DD_HH-MM-SS>/\` (the commit it ran against is in that run's own README.md, not the directory name);
\`latest/\` always mirrors the most recent run's files at a stable path.

## Run this yourself

\`\`\`
sudo ./deploy/measure.sh 120                       # 120s, 1 sample/sec, current node agent
sudo systemctl stop nodelet.service
sudo bash deploy/lib/upstream-kubelet.sh start      # swap in a real kubelet
sudo ./deploy/measure.sh 120 /tmp/out /usr/local/bin/kubelet
python3 deploy/lib/render-profiling-charts.py /tmp/charts \\
    --series "nodelet=/tmp/nodelet-out/nodelet-timeseries.csv" \\
    --series "upstream kubelet=/tmp/out/nodelet-timeseries.csv" \\
    --summary "nodelet=/tmp/nodelet-out/summary.txt" \\
    --summary "upstream kubelet=/tmp/out/summary.txt"
\`\`\`

Or dispatch the real workflow: \`gh workflow run profiling.yml\`
(\`.github/workflows/profiling.yml\`).
EOF
    exit 0
fi

# ── Per-run report mode (history/<sha>-<stamp>/README.md) ───────────────────
#
# render-profiling-charts.py already did the real aggregation work (mean,
# min–max range, and the full per-replicate breakdown across the 3 runs
# per agent) and wrote it as a ready-to-embed markdown fragment
# (stats.md) — this just wraps that in the surrounding narrative and
# links, rather than re-deriving the same numbers a second time in bash.

STATS_MD="${1:?usage: profiling-report.sh <stats_md_path> <commit_sha> <run_url>}"
COMMIT_SHA="${2:?}"
RUN_URL="${3:?}"
SHORT_SHA="${COMMIT_SHA:0:12}"

cat <<EOF
# Idle resource footprint: nodelet vs a real upstream kubelet

Three independent, identically-provisioned GitHub Actions runners per
agent (6 total), measured in parallel — same stripped \`k3s server
--disable-agent\` control plane, same containerd on each, only the node
agent itself different (see \`deploy/lib/upstream-kubelet.sh\`: a real,
standalone kubelet binary version-matched to that runner's own
k3s-embedded Kubernetes release, not a default k3s install's embedded,
unisolatable kubelet goroutine). 3 replicates per agent so one noisy run
(a GC cycle landing inside vs. just outside the window, a slow syscall, a
noisy-neighbor VM host) can't skew the conclusion — the table below shows
the real mean and min–max range across replicates, not a single point
estimate.

Commit \`$SHORT_SHA\` · sample window ${SAMPLE_SECS:-120}s, 1 sample/sec, 3 replicates/agent · [workflow run]($RUN_URL)

EOF

if [[ -s "$STATS_MD" ]]; then
    cat "$STATS_MD"
else
    echo "*(aggregated stats table unavailable — see the individual summary.txt files linked below)*"
fi

cat <<EOF

## Over time

![RSS over time](rss-over-time.png)

![CPU % over time](cpu-over-time.png)

Shaded band = min–max across the 3 replicates, line = mean. Full
1-second-resolution data behind these charts is in the linked raw CSVs
below.

## Raw data

3 replicates per agent, each with its own node-agent timeseries, control-
plane (\`k3s server\`) timeseries for cross-checking the control plane
itself measured the same regardless of which node agent it was paired
with, and full \`deploy/measure.sh\` output:

All under \`raw-data/\` (kept separate from this rendered report so it's
trivial to take just the numbers and build your own analysis):

- \`raw-data/nodelet-1\`/\`nodelet-2\`/\`nodelet-3\`-{timeseries,k3s-server-timeseries,summary}
- \`raw-data/kubelet-1\`/\`kubelet-2\`/\`kubelet-3\`-{timeseries,k3s-server-timeseries,summary}
EOF
