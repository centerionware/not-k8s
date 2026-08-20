#!/usr/bin/env bash
# profiling-workload.sh — spin a fixed synthetic workload up or down (N
# namespaces, one pod each) while deploy/measure.sh samples RSS/CPU
# concurrently, and time how long the real operation actually took.
#
# This is the "workload" profiling mode's building block
# (.github/workflows/profiling.yml's `profile_mode: workload` leg) —
# additive to the existing idle-only measurement, not a replacement for it.
# idle mode still runs deploy/measure.sh directly, unchanged.
#
# Usage:
#   profiling-workload.sh up   <count> <out_dir> <agent_pattern> [max_secs]
#   profiling-workload.sh down <count> <out_dir> <agent_pattern> [max_secs]
#
#   count           number of namespaces (one pod each) to create/delete.
#   out_dir         written with the same files deploy/measure.sh always
#                   writes (<slot>-timeseries.csv, summary.txt), plus:
#                     duration_seconds.txt   precise wall-clock time (date
#                                             +%s.%N, not measure.sh's 1s
#                                             sampling granularity) the up/
#                                             down operation itself took
#                     phase.txt              "up" or "down"
#   agent_pattern   forwarded to measure.sh's node-agent slot (see its own
#                   header) — "nodelet" or a kubelet pattern.
#   max_secs        safety cap on the concurrent measure.sh sampling window;
#                   the real up/down operation is expected to finish well
#                   inside it. Default 300s.
#
# Env:
#   PROFILING_WORKLOAD_IMAGE   pod image (default nginx:1.27-alpine) — a
#                              real, if small, webserver rather than a bare
#                              `sleep` container: it actually listens on a
#                              port and does its own idle housekeeping, so
#                              the "idle with workload present" phase
#                              reflects a workload closer to what a real
#                              cluster runs, not the emptiest possible pod.
set -euo pipefail

ACTION="${1:?usage: profiling-workload.sh up|down <count> <out_dir> <agent_pattern> [max_secs]}"
COUNT="${2:?usage: profiling-workload.sh up|down <count> <out_dir> <agent_pattern> [max_secs]}"
OUT_DIR="${3:?usage: profiling-workload.sh up|down <count> <out_dir> <agent_pattern> [max_secs]}"
AGENT_PATTERN="${4:-nodelet}"
MAX_SECS="${5:-300}"

[[ "$ACTION" == "up" || "$ACTION" == "down" ]] || {
    echo "unknown action: $ACTION (expected 'up' or 'down')" >&2
    exit 2
}
[[ "$COUNT" =~ ^[1-9][0-9]*$ ]] || {
    echo "count must be a positive integer: $COUNT" >&2
    exit 2
}

NS_PREFIX="profiling-wl"
POD_IMAGE="${PROFILING_WORKLOAD_IMAGE:-nginx:1.27-alpine}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

mkdir -p "$OUT_DIR"

# A private stop-file measure.sh polls for once per second (its own
# MEASURE_STOP_FILE contract — see deploy/measure.sh's header) so the
# concurrent sampling window ends the instant the real work below finishes,
# not after the full max_secs safety cap.
STOP_FILE="$(mktemp -u "${TMPDIR:-/tmp}/profiling-workload-stop.XXXXXX")"
rm -f "$STOP_FILE"
cleanup() { rm -f "$STOP_FILE"; }
trap cleanup EXIT

echo "==> starting concurrent measure.sh sampling (cap ${MAX_SECS}s, stop-file driven)"
MEASURE_STOP_FILE="$STOP_FILE" "$REPO_ROOT/measure.sh" "$MAX_SECS" "$OUT_DIR" "$AGENT_PATTERN" \
    > "$OUT_DIR/measure-console.txt" 2>&1 &
MEASURE_PID=$!

# Give measure.sh a moment to do its own process discovery before the timed
# region starts, so a slow `pgrep` sweep on a loaded runner doesn't get
# folded into the up/down duration being measured.
sleep 2

START="$(date +%s.%N)"

case "$ACTION" in
    up)
        echo "==> creating $COUNT namespace(s), one pod each ($POD_IMAGE)"
        for i in $(seq 0 $((COUNT - 1))); do
            ns="${NS_PREFIX}-${i}"
            kubectl create namespace "$ns" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
            kubectl apply -n "$ns" -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: workload
  namespace: $ns
spec:
  containers:
    - name: app
      image: $POD_IMAGE
      ports:
        - containerPort: 80
EOF
        done
        # "healthy" = kubectl's own Ready condition, the same signal a real
        # user waits on. Applied concurrently above, waited on sequentially
        # here — the wait is not what serializes the work, the pods are
        # already progressing in parallel by the time this loop starts.
        for i in $(seq 0 $((COUNT - 1))); do
            ns="${NS_PREFIX}-${i}"
            kubectl wait --for=condition=Ready "pod/workload" -n "$ns" --timeout="${MAX_SECS}s" >/dev/null
        done
        ;;
    down)
        echo "==> deleting $COUNT namespace(s)"
        for i in $(seq 0 $((COUNT - 1))); do
            ns="${NS_PREFIX}-${i}"
            kubectl delete namespace "$ns" --wait=false >/dev/null 2>&1 || true
        done
        # kubectl wait --for=delete needs the object to still exist at call
        # time to watch it disappear, which races the --wait=false deletes
        # above; polling for absence is simpler and race-free.
        for i in $(seq 0 $((COUNT - 1))); do
            ns="${NS_PREFIX}-${i}"
            while kubectl get namespace "$ns" >/dev/null 2>&1; do
                sleep 1
            done
        done
        ;;
esac

END="$(date +%s.%N)"
DURATION="$(awk -v a="$START" -v b="$END" 'BEGIN { printf "%.3f", b - a }')"

touch "$STOP_FILE"
wait "$MEASURE_PID" || true

echo "$ACTION" > "$OUT_DIR/phase.txt"
echo "$DURATION" > "$OUT_DIR/duration_seconds.txt"
echo "$COUNT" > "$OUT_DIR/count.txt"

echo ""
echo "==> $ACTION of $COUNT namespace(s)/pod(s) took ${DURATION}s"
cat "$OUT_DIR/summary.txt" 2>/dev/null || true
