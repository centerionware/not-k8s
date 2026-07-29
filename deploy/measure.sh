#!/usr/bin/env bash
# measure.sh — Measure idle CPU% and RSS for the k3s control plane + nodelet.
#
# Samples process stats over a configurable window (default 30s) and prints
# average CPU% and peak RSS for each component, plus combined totals.
#
# This is the key measurement script: not-k8s aims for dramatically lower
# idle overhead than stock k3s.  Baseline comparison targets:
#
#   Stock k3s (single-node, idle):
#     - k3s server + agent:  ~150-400 MB RSS combined
#     - Idle CPU:            ~30-50% of a single core (PLEG, cAdvisor, informer resyncs)
#
#   not-k8s target (stripped control plane + nodelet, idle):
#     - k3s server (no agent): ~80-150 MB RSS
#     - nodelet (mock):        ~5-15 MB RSS
#     - Idle CPU:              well under 5% of a core combined
#
# Usage:
#   ./deploy/measure.sh            # 30s sample window
#   ./deploy/measure.sh 60         # 60s sample window
#
set -euo pipefail

SAMPLE_SECS="${1:-30}"

# ── Helper: find PID by process pattern ──────────────────────────────────────

find_pid() {
    local pattern="$1"
    # Use pgrep with full command-line matching; take the oldest match (parent).
    pgrep -fo "$pattern" 2>/dev/null || true
}

# ── Helper: read RSS from /proc ──────────────────────────────────────────────

get_rss_kb() {
    local pid="$1"
    # VmRSS in /proc/<pid>/status is in kB.
    awk '/^VmRSS:/ { print $2 }' "/proc/$pid/status" 2>/dev/null || echo "0"
}

# ── Helper: read CPU ticks from /proc/<pid>/stat ─────────────────────────────
# Fields 14 (utime) and 15 (stime) are in clock ticks.

get_cpu_ticks() {
    local pid="$1"
    local stat
    stat="$(cat "/proc/$pid/stat" 2>/dev/null)" || { echo "0"; return; }
    # stat fields are space-separated; field 1 is pid, field 2 is (comm) which
    # may contain spaces, so we strip everything up to the last ')'.
    local after_comm="${stat##*) }"
    # Fields after comm: state(3) ppid(4) ... utime(14-2=12) stime(15-2=13)
    # after_comm starts at field 3, so utime is word 12, stime is word 13
    # (fields 14 and 15 counting from 1, minus the 2 we stripped = indices 12,13)
    local utime stime
    utime="$(echo "$after_comm" | awk '{print $12}')"
    stime="$(echo "$after_comm" | awk '{print $13}')"
    echo $(( utime + stime ))
}

# ── Discover PIDs ────────────────────────────────────────────────────────────

K3S_PID="$(find_pid 'k3s server')"
NODELET_PID="$(find_pid 'nodelet')"

if [[ -z "$K3S_PID" && -z "$NODELET_PID" ]]; then
    echo "ERROR: Neither k3s server nor nodelet process found." >&2
    echo "       Start the control plane and nodelet first." >&2
    exit 1
fi

echo "==> Process discovery:"
[[ -n "$K3S_PID" ]]   && echo "    k3s server PID: $K3S_PID"   || echo "    k3s server: NOT RUNNING"
[[ -n "$NODELET_PID" ]] && echo "    nodelet PID:    $NODELET_PID" || echo "    nodelet:    NOT RUNNING"
echo ""

CLK_TCK="$(getconf CLK_TCK)"

# ── Try pidstat first (more accurate) ───────────────────────────────────────

use_pidstat=false
if command -v pidstat &>/dev/null; then
    use_pidstat=true
fi

if $use_pidstat; then
    echo "==> Using pidstat for CPU measurement (${SAMPLE_SECS}s sample)..."
    echo ""

    # Build the PID list for pidstat.
    pid_args=()
    [[ -n "$K3S_PID" ]]     && pid_args+=("-p" "$K3S_PID")
    [[ -n "$NODELET_PID" ]] && pid_args+=("-p" "$NODELET_PID")

    # pidstat -u: CPU utilization.  Sample every 2s for the window.
    # -h: horizontal/parseable output.
    INTERVALS=$(( SAMPLE_SECS / 2 ))
    (( INTERVALS < 1 )) && INTERVALS=1

    # Capture pidstat output.  We'll parse the "Average:" lines.
    PIDSTAT_OUT="$(pidstat -u -h "${pid_args[@]}" 2 "$INTERVALS" 2>/dev/null)" || true

    echo "── CPU (pidstat average over ${SAMPLE_SECS}s) ──"
    echo ""

    parse_pidstat_cpu() {
        local pid="$1"
        local label="$2"
        # Average lines look like:  ... <PID> ... %usr %system %guest %wait %CPU ...
        local cpu
        cpu="$(echo "$PIDSTAT_OUT" | awk -v pid="$pid" '/Average:/ && $0 ~ pid { for(i=1;i<=NF;i++) if($i==pid) print $(i+6) }' | tail -1)"
        if [[ -z "$cpu" ]]; then
            cpu="$(echo "$PIDSTAT_OUT" | grep "Average:" | awk -v pid="$pid" '$0 ~ pid {print $8}' | tail -1)"
        fi
        echo "${cpu:-N/A}"
    }

    K3S_CPU="N/A"
    NODELET_CPU_PCT="N/A"
    [[ -n "$K3S_PID" ]]     && K3S_CPU="$(parse_pidstat_cpu "$K3S_PID" "k3s")"
    [[ -n "$NODELET_PID" ]] && NODELET_CPU_PCT="$(parse_pidstat_cpu "$NODELET_PID" "nodelet")"

else
    # ── Fallback: manual /proc sampling ──────────────────────────────────────
    echo "==> pidstat not found; falling back to /proc/stat sampling (${SAMPLE_SECS}s)..."
    echo ""

    # Snapshot CPU ticks at start.
    K3S_TICKS_START=0
    NODELET_TICKS_START=0
    [[ -n "$K3S_PID" ]]     && K3S_TICKS_START="$(get_cpu_ticks "$K3S_PID")"
    [[ -n "$NODELET_PID" ]] && NODELET_TICKS_START="$(get_cpu_ticks "$NODELET_PID")"

    sleep "$SAMPLE_SECS"

    # Snapshot CPU ticks at end.
    K3S_TICKS_END=0
    NODELET_TICKS_END=0
    [[ -n "$K3S_PID" ]]     && K3S_TICKS_END="$(get_cpu_ticks "$K3S_PID")"
    [[ -n "$NODELET_PID" ]] && NODELET_TICKS_END="$(get_cpu_ticks "$NODELET_PID")"

    # CPU% = (delta_ticks / (sample_secs * CLK_TCK)) * 100
    calc_cpu_pct() {
        local start="$1" end="$2"
        local delta=$(( end - start ))
        # Use awk for floating-point.
        awk -v d="$delta" -v s="$SAMPLE_SECS" -v c="$CLK_TCK" \
            'BEGIN { printf "%.2f", (d / (s * c)) * 100 }'
    }

    K3S_CPU="0.00"
    NODELET_CPU_PCT="0.00"
    [[ -n "$K3S_PID" ]]     && K3S_CPU="$(calc_cpu_pct "$K3S_TICKS_START" "$K3S_TICKS_END")"
    [[ -n "$NODELET_PID" ]] && NODELET_CPU_PCT="$(calc_cpu_pct "$NODELET_TICKS_START" "$NODELET_TICKS_END")"

    echo "── CPU (/proc/stat delta over ${SAMPLE_SECS}s) ──"
    echo ""
fi

# ── RSS measurement (always from /proc) ──────────────────────────────────────
# Sample RSS multiple times over the window and report peak.

sample_peak_rss() {
    local pid="$1"
    local peak=0
    local samples=$(( SAMPLE_SECS / 2 ))
    (( samples < 1 )) && samples=1

    for (( i=0; i<samples; i++ )); do
        local rss
        rss="$(get_rss_kb "$pid")"
        (( rss > peak )) && peak=$rss
        # Only sleep if we're not using pidstat (pidstat already waited).
        if ! $use_pidstat; then
            break  # /proc fallback already slept; just take one reading.
        fi
        # If using pidstat, pidstat already slept.  Take one reading post-wait.
        break
    done

    # Also take a current reading.
    local current
    current="$(get_rss_kb "$pid")"
    (( current > peak )) && peak=$current
    echo "$peak"
}

K3S_RSS_KB=0
NODELET_RSS_KB=0
[[ -n "$K3S_PID" ]]     && K3S_RSS_KB="$(sample_peak_rss "$K3S_PID")"
[[ -n "$NODELET_PID" ]] && NODELET_RSS_KB="$(sample_peak_rss "$NODELET_PID")"

# Convert to MB.
K3S_RSS_MB="$(awk -v kb="$K3S_RSS_KB" 'BEGIN { printf "%.1f", kb / 1024 }')"
NODELET_RSS_MB="$(awk -v kb="$NODELET_RSS_KB" 'BEGIN { printf "%.1f", kb / 1024 }')"

# ── Combined totals ──────────────────────────────────────────────────────────

COMBINED_CPU="$(awk -v a="${K3S_CPU//N\/A/0}" -v b="${NODELET_CPU_PCT//N\/A/0}" \
    'BEGIN { printf "%.2f", a + b }')"
COMBINED_RSS_MB="$(awk -v a="$K3S_RSS_MB" -v b="$NODELET_RSS_MB" \
    'BEGIN { printf "%.1f", a + b }')"

# ── Report ───────────────────────────────────────────────────────────────────

printf "  %-20s %10s %12s\n" "PROCESS" "CPU %" "RSS (MB)"
printf "  %-20s %10s %12s\n" "────────────────────" "──────────" "────────────"

if [[ -n "$K3S_PID" ]]; then
    printf "  %-20s %10s %12s\n" "k3s server" "${K3S_CPU}%" "$K3S_RSS_MB"
fi
if [[ -n "$NODELET_PID" ]]; then
    printf "  %-20s %10s %12s\n" "nodelet" "${NODELET_CPU_PCT}%" "$NODELET_RSS_MB"
fi

printf "  %-20s %10s %12s\n" "────────────────────" "──────────" "────────────"
printf "  %-20s %10s %12s\n" "COMBINED" "${COMBINED_CPU}%" "$COMBINED_RSS_MB"

echo ""
echo "── Reference: stock k3s idle baseline ──"
echo ""
echo "  Stock k3s (single node, idle, no workloads):"
echo "    CPU:  ~30-50% of one core  (PLEG polling, cAdvisor housekeeping, informer resyncs)"
echo "    RSS:  ~150-400 MB combined (server + agent + containerd)"
echo ""
echo "  not-k8s target:"
echo "    CPU:  <5% of one core combined"
echo "    RSS:  <100 MB combined (stripped server + nodelet)"
echo ""

if awk -v c="$COMBINED_CPU" 'BEGIN { exit (c < 5.0) ? 0 : 1 }'; then
    echo "  ==> PASS: Combined idle CPU is under 5%.  Looking good."
else
    echo "  ==> NOTE: Combined idle CPU is above 5%.  Investigate what's hot."
    echo "           Try: RUST_LOG=nodelet=debug to check reconcile frequency."
fi
