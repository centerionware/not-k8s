#!/usr/bin/env bash
# profiling-workload-report.sh — render the "workload" profiling mode's
# nodelet-vs-real-upstream-kubelet comparison as GitHub-flavored markdown:
# time to spin 10 pods/namespaces up, 120s idle with them running, and time
# to tear them back down, each with its own CPU/RSS numbers.
#
# Sibling to profiling-report.sh (idle-only mode, unchanged) — this is the
# additive workload-mode report, not a replacement.
#
# Two modes, same shape as profiling-report.sh:
#   profiling-workload-report.sh <report_dir> <commit_sha> <run_url> \
#       <pod_count> <sample_secs>
#       Renders one run's README.md. Assumes <report_dir>/{spinup,
#       idle-workload,teardown}/{stats.md,rss-over-time.png,
#       cpu-over-time.png} already exist (render-profiling-charts.py's job),
#       and {spinup,teardown}/duration.md too.
#   profiling-workload-report.sh --readme <commit_sha> <run_url> <run_stamp>
#       Renders the profiling-results branch's workload/README.md, pointing
#       at the latest workload run.
set -euo pipefail

if [[ "${1:-}" == "--readme" ]]; then
    COMMIT_SHA="${2:?usage: profiling-workload-report.sh --readme <commit_sha> <run_url> <run_stamp>}"
    RUN_URL="${3:?}"
    RUN_STAMP="${4:?}"
    SHORT_SHA="${COMMIT_SHA:0:12}"

    cat <<EOF
# not-k8s workload profiling results

Measured **nodelet** vs a **real, standalone upstream kubelet** across a
full spin-up / idle-with-workload / spin-down cycle of a fixed synthetic
workload, on the same stripped \`k3s server --disable-agent\` control plane
and the same containerd — only the node agent differs. Sibling to
[../README.md](../README.md) (idle-only steady-state comparison).

## Latest result

**[latest/README.md](latest/README.md)** — commit \`$SHORT_SHA\`, [workflow run]($RUN_URL).

## Methodology

- \`.github/workflows/profiling.yml\`, dispatched with \`profile_mode: workload\`.
  Same 6-runner-in-parallel, 3-replicates-per-agent, real-standalone-installer
  setup as idle mode (see ../README.md) — only what's measured after bootstrap
  differs.
- **Spin up**: \`deploy/lib/profiling-workload.sh up\` creates N namespaces
  (default 10), one pod each, and waits for every pod's \`Ready\` condition —
  timed with a precise wall-clock stopwatch, not sampling-loop granularity.
  \`deploy/measure.sh\` samples RSS/CPU concurrently, stopping the instant the
  last pod goes Ready rather than running a fixed window.
- **Idle with workload present**: the same fixed-window \`deploy/measure.sh\`
  sample idle mode uses, just with the N pods now running.
- **Spin down**: mirrors spin-up — all N namespaces deleted, timed until
  every one is actually gone, sampled concurrently the same way.
- A final job downloads all runners' data, aggregates the 3 replicates per
  agent (mean, min–max range) per phase, and renders the charts + report.

Every run's full output is kept at \`history/<YYYY-MM-DD_HH-MM-SS>/\`;
\`latest/\` mirrors the most recent one.

## Run this yourself

\`\`\`
sudo bash deploy/lib/profiling-workload.sh up   10 /tmp/spinup   nodelet
sudo ./deploy/measure.sh 120 /tmp/idle nodelet
sudo bash deploy/lib/profiling-workload.sh down 10 /tmp/teardown nodelet
\`\`\`

Or dispatch \`gh workflow run profiling.yml -f profile_mode=workload\`.
EOF
    exit 0
fi

REPORT_DIR="${1:?usage: profiling-workload-report.sh <report_dir> <commit_sha> <run_url> <pod_count> <sample_secs>}"
COMMIT_SHA="${2:?}"
RUN_URL="${3:?}"
POD_COUNT="${4:?}"
SAMPLE_SECS="${5:?}"
SHORT_SHA="${COMMIT_SHA:0:12}"

phase_section() {
    local dir="$1" title="$2" duration_note="$3"
    echo "## $title"
    echo ""
    if [[ -s "$REPORT_DIR/$dir/duration.md" ]]; then
        echo "$duration_note"
        echo ""
        cat "$REPORT_DIR/$dir/duration.md"
        echo ""
    fi
    if [[ -s "$REPORT_DIR/$dir/stats.md" ]]; then
        cat "$REPORT_DIR/$dir/stats.md"
    else
        echo "*(aggregated stats table unavailable — see raw-data/ summary.txt files)*"
    fi
    echo ""
    if [[ -s "$REPORT_DIR/$dir/rss-over-time.png" ]]; then
        echo "![RSS over time]($dir/rss-over-time.png)"
        echo ""
    fi
    if [[ -s "$REPORT_DIR/$dir/cpu-over-time.png" ]]; then
        echo "![CPU % over time]($dir/cpu-over-time.png)"
        echo ""
    fi
}

cat <<EOF
# Workload profiling: nodelet vs a real upstream kubelet

Same stripped \`k3s server --disable-agent\` control plane, same containerd
— only the node agent differs. $POD_COUNT namespaces (one pod each) spun up,
${SAMPLE_SECS}s idle with them running, then spun back down. 3 independent,
identically-provisioned runners per agent (6 total, measured in parallel);
each table below is the real mean and min–max range across replicates.

Commit \`$SHORT_SHA\` · $POD_COUNT pods · idle window ${SAMPLE_SECS}s, 1 sample/sec, 3 replicates/agent · [workflow run]($RUN_URL)

EOF

phase_section spinup "Spin-up: creating $POD_COUNT pods/namespaces" \
    "**Time to healthy** — wall-clock from first \`kubectl apply\` to every pod's \`Ready\` condition:"

phase_section idle-workload "Idle, with $POD_COUNT pods running" \
    ""

phase_section teardown "Spin-down: deleting $POD_COUNT pods/namespaces" \
    "**Time to gone** — wall-clock from first \`kubectl delete\` to every namespace actually removed:"

cat <<EOF
## Raw data

Full 1-second-resolution CSVs, precise phase-duration readings, and
\`deploy/measure.sh\`'s complete human-readable + machine-readable output,
per replicate, under \`raw-data/\`:

- \`raw-data/{spinup,idle-workload,teardown}/nodelet-1\`/\`nodelet-2\`/\`nodelet-3\`-{timeseries,summary,duration}
- \`raw-data/{spinup,idle-workload,teardown}/kubelet-1\`/\`kubelet-2\`/\`kubelet-3\`-{timeseries,summary,duration}
EOF
