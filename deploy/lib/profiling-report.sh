#!/usr/bin/env bash
# profiling-report.sh — render a nodelet-vs-real-upstream-kubelet idle
# resource-footprint comparison as GitHub-flavored markdown, embedding the
# real PNG charts render-profiling-charts.py already built from measure.sh's
# per-second CSV time series.
#
# Two modes:
#   profiling-report.sh <stats_md_path> <commit_sha> <run_url>
#       Renders one run's README.md (the main file in its
#       history/<timestamp>/ folder — see profiling.yml's publish step).
#       Assumes rss-over-time.png/cpu-over-time.png already exist
#       alongside wherever this output gets written
#       (render-profiling-charts.py's job, not this script's).
#   profiling-report.sh --readme <commit_sha> <run_url> <run_stamp>
#       Renders the profiling-results branch's top-level README.md,
#       pointing at the latest run.
#
# Keep this factual and short: what was measured, how, where the numbers
# are — not an essay justifying every design choice.
set -euo pipefail

# ── --readme mode (profiling-results branch's top-level README.md) ──────────

if [[ "${1:-}" == "--readme" ]]; then
    COMMIT_SHA="${2:?usage: profiling-report.sh --readme <commit_sha> <run_url> <run_stamp>}"
    RUN_URL="${3:?}"
    RUN_STAMP="${4:?}"
    SHORT_SHA="${COMMIT_SHA:0:12}"

    cat <<EOF
# not-k8s profiling results

Measured idle resource footprint: **nodelet** vs a **real, standalone
upstream kubelet**, on the same stripped \`k3s server --disable-agent\`
control plane and the same containerd — only the node agent differs.

## Latest result

**[latest/README.md](latest/README.md)** — commit \`$SHORT_SHA\`, [workflow run]($RUN_URL).

![RSS over time](latest/rss-over-time.png)
![CPU % over time](latest/cpu-over-time.png)

## Methodology

- 6 GitHub Actions runners, provisioned identically and in parallel: 3
  replicates each of \`agent: [nodelet, kubelet]\`
  (\`.github/workflows/profiling.yml\`).
- Every runner installs via the real standalone installer (the
  \`install-scripts\` branch's \`install.sh\`, \`--with-cri\`) — the actual
  published release binary, not a from-source build. Nodelet legs stop
  there. Kubelet legs add \`--skip-nodelet\`: nodelet is never built,
  installed, or started on those runners at all — only the control plane
  + containerd + CNI come up, and \`deploy/lib/upstream-kubelet.sh\` then
  installs a real kubelet binary, version-matched to that runner's own
  k3s-embedded Kubernetes release, against that same stack.
- After a 30s settle, \`deploy/measure.sh\` samples RSS and CPU **every
  second** for the sample window, per process, writing a real per-second
  CSV. CPU-seconds (real processor time consumed) is the primary CPU
  metric, not %, which is noisy at near-idle utilization — perf's
  \`task-clock\` is used for sub-millisecond precision where available,
  falling back to \`/proc\`-derived ticks otherwise. Real hardware
  cycles/instructions are captured via \`perf stat\` when the runner's
  \`perf_event\` access allows it — confirmed unavailable on GitHub's own
  hosted runners (no PMU passthrough to the guest for attach-mode
  profiling), so this is usually N/A there.
- A final job downloads all 6 runners' data, aggregates the 3 replicates
  per agent (mean, min–max range), and renders the charts + report
  (\`deploy/lib/render-profiling-charts.py\`, \`deploy/lib/profiling-report.sh\`).

Every run's full output (report, charts, and all raw per-replicate CSVs
under \`raw-data/\`) is kept at \`history/<YYYY-MM-DD_HH-MM-SS>/\`; \`latest/\`
mirrors the most recent one.

## Run this yourself

\`\`\`
sudo ./deploy/measure.sh 120                             # current node agent
sudo systemctl stop nodelet.service
sudo bash deploy/lib/upstream-kubelet.sh start            # swap in a real kubelet
sudo ./deploy/measure.sh 120 /tmp/out /usr/local/bin/kubelet
\`\`\`

Or dispatch \`gh workflow run profiling.yml\`.
EOF
    exit 0
fi

# ── Per-run report mode (history/<timestamp>/README.md) ─────────────────────
#
# render-profiling-charts.py already did the aggregation (mean, min–max
# range, full per-replicate breakdown) and wrote it as a ready-to-embed
# markdown fragment (stats.md) — this wraps that with the charts and raw
# data links.

STATS_MD="${1:?usage: profiling-report.sh <stats_md_path> <commit_sha> <run_url>}"
COMMIT_SHA="${2:?}"
RUN_URL="${3:?}"
SHORT_SHA="${COMMIT_SHA:0:12}"

cat <<EOF
# Idle resource footprint: nodelet vs a real upstream kubelet

Same stripped \`k3s server --disable-agent\` control plane, same containerd
— only the node agent differs. 3 independent, identically-provisioned
runners per agent (6 total, measured in parallel); the table below is the
real mean and min–max range across replicates, not a single point
estimate.

Commit \`$SHORT_SHA\` · sample window ${SAMPLE_SECS:-120}s, 1 sample/sec, 3 replicates/agent · [workflow run]($RUN_URL)

EOF

if [[ -s "$STATS_MD" ]]; then
    cat "$STATS_MD"
else
    echo "*(aggregated stats table unavailable — see the individual summary.txt files in raw-data/)*"
fi

cat <<EOF

## Over time

![RSS over time](rss-over-time.png)

![CPU % over time](cpu-over-time.png)

Shaded band = min–max across the 3 replicates, line = mean.

## Raw data

Full 1-second-resolution CSVs and \`deploy/measure.sh\`'s complete
human-readable + machine-readable output, per replicate, under
\`raw-data/\`:

- \`raw-data/nodelet-1\`/\`nodelet-2\`/\`nodelet-3\`-{timeseries,k3s-server-timeseries,summary}
- \`raw-data/kubelet-1\`/\`kubelet-2\`/\`kubelet-3\`-{timeseries,k3s-server-timeseries,summary}
EOF
