#!/usr/bin/env bash
# measure.sh — Measure idle CPU%/CPU-seconds/RSS *over time* (1-second
# samples, not just a single before/after delta), and — where the runner's
# perf_event access allows it — real hardware cycles/instructions, for the
# k3s control-plane process and nodelet.
#
# Round 124: rewritten twice in the same round after a published CI report
# came back showing 0.00% CPU for *both* processes, traced to real gaps:
#   1. It silently fell back from pidstat to a single before/after
#      /proc/<pid>/stat delta whenever `pidstat` (the `sysstat` package)
#      wasn't installed — true by default on GitHub's ubuntu-latest
#      runners. That single-delta approach needs at least one full CLK_TCK
#      (usually 10ms) of combined CPU time across the *whole* window to
#      register as anything but exactly zero.
#   2. The old 30s window was short enough to plausibly miss every
#      periodic cycle (status push, GC interval, informer resync) on an
#      otherwise-empty idle cluster, and only ever reported one final
#      average — no way to see *when* activity happened.
# Second pass: dropped the pidstat/sysstat dependency entirely in favor of
# a self-contained per-second /proc sampling loop (finer-grained than
# pidstat's own 2s default, and needs nothing installed on any Linux
# runner), and now emits a real per-second time series (CSV) per process
# instead of only a final summary, so a report can chart what actually
# happened over the whole window instead of one number.
#
# IMPORTANT caveat this script's own output makes explicit rather than
# implying false precision: k3s runs kubelet (when the agent isn't
# disabled) as an embedded goroutine inside the *same* OS process as the
# apiserver/etcd/controller-manager/scheduler — there is no separate
# "kubelet" binary/process to isolate the way a vanilla kubeadm cluster
# has. So on stock k3s, the "k3s server" row/series below is the *entire*
# stack combined (control plane + kubelet + kube-proxy + flannel), not
# kubelet alone. nodelet, by contrast, genuinely is its own separate
# process on both sides. Don't read the stock-side number as "kubelet's
# own number."
#
# Per-process outputs (written under --out-dir, default a fresh mktemp -d):
#   <name>-timeseries.csv   second,rss_kb,cpu_pct — the real per-second data
#   summary.txt             human-readable table + MEASURE_* machine block
#     (cycles/instructions/IPC in the summary are a single whole-window
#     aggregate from perf, not a per-second series — see the perf section
#     below for why)
#
# Usage:
#   ./deploy/measure.sh                          # 30s sample window
#   ./deploy/measure.sh 120                       # 120s sample window
#   ./deploy/measure.sh 120 /tmp/measure-out       # explicit output directory
#
set -uo pipefail

SAMPLE_SECS="${1:-30}"
OUT_DIR="${2:-}"
# Round 124: the node-agent side of the comparison is nodelet by default,
# but the same script also drives the upstream-kubelet.sh profiling phase
# (deploy/lib/upstream-kubelet.sh) -- pass a different process-match
# pattern (e.g. "kubelet") as $3 to measure that agent instead. Internal
# variable names stay NODELET_* either way (this file's job is "the
# non-control-plane node agent slot," not literally nodelet specifically).
AGENT_PATTERN="${3:-nodelet}"
[[ -n "$OUT_DIR" ]] || OUT_DIR="$(mktemp -d /tmp/not-k8s-measure.XXXXXX)"
mkdir -p "$OUT_DIR"

find_pid() {
    pgrep -fo "$1" 2>/dev/null || true
}
get_rss_kb() {
    awk '/^VmRSS:/ { print $2 }' "/proc/$1/status" 2>/dev/null || echo "0"
}
get_cpu_ticks() {
    local pid="$1" stat after_comm
    stat="$(cat "/proc/$pid/stat" 2>/dev/null)" || { echo "0"; return; }
    after_comm="${stat##*) }"
    echo $(( $(echo "$after_comm" | awk '{print $12}') + $(echo "$after_comm" | awk '{print $13}') ))
}

K3S_PID="$(find_pid 'k3s server')"
NODELET_PID="$(find_pid "$AGENT_PATTERN")"

if [[ -z "$K3S_PID" && -z "$NODELET_PID" ]]; then
    echo "ERROR: Neither k3s server nor nodelet process found." >&2
    echo "       Start the control plane and nodelet first." >&2
    exit 1
fi

echo "==> Process discovery:"
[[ -n "$K3S_PID" ]]     && echo "    k3s server PID: $K3S_PID"     || echo "    k3s server: NOT RUNNING"
[[ -n "$NODELET_PID" ]] && echo "    $AGENT_PATTERN PID:    $NODELET_PID" || echo "    $AGENT_PATTERN:    NOT RUNNING"
echo "    output directory: $OUT_DIR"
echo ""

CLK_TCK="$(getconf CLK_TCK)"

# ── Test hardware, for context on every report this feeds ───────────────────
# Round 124: results are only meaningful relative to the specific hardware
# they ran on (GitHub-hosted runners aren't a fixed spec -- Azure picks
# whatever's available in a pool at dispatch time), so record it rather
# than leaving a reader to assume/guess. x86 cpuinfo has "model name";
# ARM's doesn't, usually just "Model" (the board) or numeric "CPU part" --
# tried in that order, falling back to "unknown" rather than a guess.
CPU_ARCH="$(uname -m)"
CPU_CORES="$(nproc 2>/dev/null || echo "?")"
CPU_MODEL="$(awk -F': *' '/^model name/ {print $2; exit}' /proc/cpuinfo 2>/dev/null)"
[[ -n "$CPU_MODEL" ]] || CPU_MODEL="$(awk -F': *' '/^Model/ {print $2; exit}' /proc/cpuinfo 2>/dev/null)"
[[ -n "$CPU_MODEL" ]] || CPU_MODEL="unknown"
echo "==> Test hardware: arch=$CPU_ARCH cores=$CPU_CORES model=\"$CPU_MODEL\""
echo ""

# ── perf availability (best-effort hardware cycles/instructions, plus a
# software task-clock reading that works even when hardware counters
# don't) ─────────────────────────────────────────────────────────────────
#
# Round 124 (found live on a real GitHub Actions runner, not just
# theorized): the original check here tested `task-clock` -- a *software*
# event the kernel always tracks itself, so it succeeds regardless of
# whether real hardware performance counters are actually reachable. That
# made PERF_OK misleadingly report "available" even on runners where
# `cycles`/`instructions` (real hardware PMU events) then silently came
# back empty every time -- confirmed for real: GitHub's hosted
# ubuntu-latest runners are virtualized (Azure), and the hypervisor
# doesn't expose real hardware PMU registers to the guest for
# attach-to-PID profiling at all, root or not. That's a genuine
# infrastructure ceiling, not something fixable by better scripting here.
# Response: request task-clock *alongside* cycles/instructions in the
# same perf stat call -- task-clock gives sub-millisecond-precision CPU
# time even when the hardware events don't report anything, so it's
# always worth asking for regardless of what PERF_OK below (now testing
# the actual hardware event, not the software one) says about hardware
# counters specifically.
PERF_OK=false
if command -v perf >/dev/null 2>&1 && perf stat -e cycles -- true >/dev/null 2>&1; then
    PERF_OK=true
fi
PERF_INSTALLED=false
command -v perf >/dev/null 2>&1 && PERF_INSTALLED=true

# perf's per-second interval mode (-I) exists, but folding its output back
# onto a per-second RSS/CPU% row by timestamp turned out to be exactly the
# kind of fragile text-join that's easy to get subtly wrong (found live
# testing this script locally: it silently truncated both timeseries CSVs
# to empty). A single whole-window aggregate read is simpler, robust, and
# still gives real cycles/instructions/task-clock numbers -- just as a
# summary total rather than its own time series.
K3S_PERF_RAW="$OUT_DIR/.k3s-perf-raw.txt"
NODELET_PERF_RAW="$OUT_DIR/.nodelet-perf-raw.txt"
K3S_PERF_BGPID=""
NODELET_PERF_BGPID=""

if $PERF_INSTALLED; then
    if $PERF_OK; then
        echo "==> perf available with real hardware counters; sampling cycles/instructions/task-clock (whole-window aggregate), concurrently with the per-second RSS/CPU loop below"
    else
        echo "==> perf is installed but hardware performance counters aren't reachable on this runner"
        echo "    (confirmed common on virtualized cloud runners -- the hypervisor doesn't expose the"
        echo "    PMU to the guest at all, root or not); still sampling task-clock, a software event"
        echo "    that gives sub-millisecond-precision CPU time regardless."
    fi
    if [[ -n "$K3S_PID" ]]; then
        perf stat -e cycles,instructions,task-clock -p "$K3S_PID" -o "$K3S_PERF_RAW" -- sleep "$SAMPLE_SECS" 2>/dev/null &
        K3S_PERF_BGPID=$!
    fi
    if [[ -n "$NODELET_PID" ]]; then
        perf stat -e cycles,instructions,task-clock -p "$NODELET_PID" -o "$NODELET_PERF_RAW" -- sleep "$SAMPLE_SECS" 2>/dev/null &
        NODELET_PERF_BGPID=$!
    fi
else
    echo "==> perf not installed; cycles/instructions/task-clock will be omitted, CPU-seconds (from /proc, ~10ms"
    echo "    resolution) and RSS remain the primary metrics."
fi
echo ""
echo "==> sampling RSS + CPU% every 1s for ${SAMPLE_SECS}s..."

K3S_TS="$OUT_DIR/k3s-timeseries.csv"
NODELET_TS="$OUT_DIR/nodelet-timeseries.csv"
echo "second,rss_kb,cpu_pct" > "$K3S_TS"
echo "second,rss_kb,cpu_pct" > "$NODELET_TS"

k3s_prev_ticks=0
nodelet_prev_ticks=0
[[ -n "$K3S_PID" ]]     && k3s_prev_ticks="$(get_cpu_ticks "$K3S_PID")"
[[ -n "$NODELET_PID" ]] && nodelet_prev_ticks="$(get_cpu_ticks "$NODELET_PID")"

k3s_rss_peak_kb=0
nodelet_rss_peak_kb=0
k3s_ticks_total=0
nodelet_ticks_total=0

for (( sec=1; sec<=SAMPLE_SECS; sec++ )); do
    sleep 1

    if [[ -n "$K3S_PID" ]] && [[ -d "/proc/$K3S_PID" ]]; then
        rss="$(get_rss_kb "$K3S_PID")"
        ticks="$(get_cpu_ticks "$K3S_PID")"
        delta=$(( ticks - k3s_prev_ticks ))
        (( delta < 0 )) && delta=0
        k3s_prev_ticks="$ticks"
        k3s_ticks_total=$(( k3s_ticks_total + delta ))
        (( rss > k3s_rss_peak_kb )) && k3s_rss_peak_kb="$rss"
        pct="$(awk -v d="$delta" -v c="$CLK_TCK" 'BEGIN { printf "%.2f", (d / c) * 100 }')"
        echo "$sec,$rss,$pct" >> "$K3S_TS"
    fi

    if [[ -n "$NODELET_PID" ]] && [[ -d "/proc/$NODELET_PID" ]]; then
        rss="$(get_rss_kb "$NODELET_PID")"
        ticks="$(get_cpu_ticks "$NODELET_PID")"
        delta=$(( ticks - nodelet_prev_ticks ))
        (( delta < 0 )) && delta=0
        nodelet_prev_ticks="$ticks"
        nodelet_ticks_total=$(( nodelet_ticks_total + delta ))
        (( rss > nodelet_rss_peak_kb )) && nodelet_rss_peak_kb="$rss"
        pct="$(awk -v d="$delta" -v c="$CLK_TCK" 'BEGIN { printf "%.2f", (d / c) * 100 }')"
        echo "$sec,$rss,$pct" >> "$NODELET_TS"
    fi
done

[[ -n "$K3S_PERF_BGPID" ]]     && wait "$K3S_PERF_BGPID" 2>/dev/null
[[ -n "$NODELET_PERF_BGPID" ]] && wait "$NODELET_PERF_BGPID" 2>/dev/null

# perf stat's default text report has lines like (exact spacing/thousands
# separators vary by version):
#   9,876,543,210      cycles:u
#  12,345,678,901      instructions:u    #    1.25  insn per cycle
# "<not supported>"/"<not counted>" means this counter isn't available on
# this hardware/virtualization -- treated as unavailable (blank), never
# fabricated as 0.
parse_perf_value() {
    local file="$1" event_pattern="$2"
    [[ -s "$file" ]] || { echo ""; return; }
    awk -v ev="$event_pattern" '
        $0 ~ ev {
            val = $1
            gsub(",", "", val)
            if (val ~ /^[0-9]+$/) { print val; exit }
        }
    ' "$file"
}
K3S_CYCLES="$(parse_perf_value "$K3S_PERF_RAW" 'cycles')"
K3S_INSTRUCTIONS="$(parse_perf_value "$K3S_PERF_RAW" 'instructions')"
NODELET_CYCLES="$(parse_perf_value "$NODELET_PERF_RAW" 'cycles')"
NODELET_INSTRUCTIONS="$(parse_perf_value "$NODELET_PERF_RAW" 'instructions')"

# task-clock is a software event (always available, even when hardware
# cycles/instructions aren't -- see the perf section above) reported in
# milliseconds with real decimal precision, unlike parse_perf_value's
# integer-only cycles/instructions parsing. Sub-millisecond CPU time, vs.
# the ~10ms resolution the /proc-tick-based CPU_SECONDS below is limited
# to by CLK_TCK -- preferred whenever perf actually captured it.
parse_perf_task_clock_ms() {
    local file="$1"
    [[ -s "$file" ]] || { echo ""; return; }
    awk '
        /task-clock/ {
            val = $1
            gsub(",", "", val)
            if (val ~ /^[0-9]+(\.[0-9]+)?$/) { print val; exit }
        }
    ' "$file"
}
K3S_TASK_CLOCK_MS="$(parse_perf_task_clock_ms "$K3S_PERF_RAW")"
NODELET_TASK_CLOCK_MS="$(parse_perf_task_clock_ms "$NODELET_PERF_RAW")"
rm -f "$K3S_PERF_RAW" "$NODELET_PERF_RAW"

# ── Summary stats derived from the time series ───────────────────────────────

summarize_cpu_avg() {
    local csv="$1"
    [[ -s "$csv" ]] || { echo "0.00"; return; }
    awk -F, 'NR>1 { n++; cpu_sum += $3 } END { printf "%.2f", (n>0 ? cpu_sum/n : 0) }' "$csv"
}
K3S_CPU_AVG="$(summarize_cpu_avg "$K3S_TS")"
NODELET_CPU_AVG="$(summarize_cpu_avg "$NODELET_TS")"

K3S_RSS_MB="$(awk -v kb="$k3s_rss_peak_kb" 'BEGIN { printf "%.1f", kb / 1024 }')"
NODELET_RSS_MB="$(awk -v kb="$nodelet_rss_peak_kb" 'BEGIN { printf "%.1f", kb / 1024 }')"

# CPU-seconds: prefer perf's task-clock (sub-millisecond precision) when
# it actually reported one; fall back to the /proc-tick delta (~10ms
# resolution, CLK_TCK-limited) otherwise. Track which source won so the
# report can say so honestly rather than implying uniform precision.
cpu_seconds_from_ticks_or_task_clock() {
    local ticks_total="$1" task_clock_ms="$2"
    if [[ -n "$task_clock_ms" ]]; then
        awk -v ms="$task_clock_ms" 'BEGIN { printf "%.6f", ms / 1000 }'
    else
        awk -v t="$ticks_total" -v c="$CLK_TCK" 'BEGIN { printf "%.3f", t / c }'
    fi
}
K3S_CPU_SECONDS="$(cpu_seconds_from_ticks_or_task_clock "$k3s_ticks_total" "$K3S_TASK_CLOCK_MS")"
NODELET_CPU_SECONDS="$(cpu_seconds_from_ticks_or_task_clock "$nodelet_ticks_total" "$NODELET_TASK_CLOCK_MS")"
K3S_CPU_SECONDS_SOURCE="$([[ -n "$K3S_TASK_CLOCK_MS" ]] && echo "perf-task-clock" || echo "proc-ticks")"
NODELET_CPU_SECONDS_SOURCE="$([[ -n "$NODELET_TASK_CLOCK_MS" ]] && echo "perf-task-clock" || echo "proc-ticks")"

calc_ipc() {
    [[ -n "$1" && -n "$2" && "$2" != "0" ]] || { echo ""; return; }
    awk -v i="$1" -v c="$2" 'BEGIN { printf "%.3f", i / c }'
}
K3S_IPC="$(calc_ipc "$K3S_INSTRUCTIONS" "$K3S_CYCLES")"
NODELET_IPC="$(calc_ipc "$NODELET_INSTRUCTIONS" "$NODELET_CYCLES")"

COMBINED_RSS_MB="$(awk -v a="$K3S_RSS_MB" -v b="$NODELET_RSS_MB" 'BEGIN { printf "%.1f", a + b }')"
COMBINED_CPU_SECONDS="$(awk -v a="$K3S_CPU_SECONDS" -v b="$NODELET_CPU_SECONDS" 'BEGIN { printf "%.3f", a + b }')"
COMBINED_CPU_AVG="$(awk -v a="$K3S_CPU_AVG" -v b="$NODELET_CPU_AVG" 'BEGIN { printf "%.2f", a + b }')"
sum_or_blank() {
    [[ -z "$1" && -z "$2" ]] && { echo ""; return; }
    awk -v a="${1:-0}" -v b="${2:-0}" 'BEGIN { printf "%.0f", a + b }'
}
COMBINED_CYCLES="$(sum_or_blank "$K3S_CYCLES" "$NODELET_CYCLES")"
COMBINED_INSTRUCTIONS="$(sum_or_blank "$K3S_INSTRUCTIONS" "$NODELET_INSTRUCTIONS")"

# ── Human-readable + machine-readable summary ───────────────────────────────

fmt_or_na() { [[ -n "$1" ]] && echo "$1" || echo "N/A"; }

{
    printf "  %-16s %10s %10s %10s %14s %14s %8s\n" "PROCESS" "avg CPU%" "CPU-sec" "RSS (MB)" "cycles" "instructions" "IPC"
    printf "  %-16s %10s %10s %10s %14s %14s %8s\n" "────────────────" "──────────" "──────────" "──────────" "──────────────" "──────────────" "────────"
    [[ -n "$K3S_PID" ]] && printf "  %-16s %10s %10s %10s %14s %14s %8s\n" "k3s server" "${K3S_CPU_AVG}%" "$K3S_CPU_SECONDS" "$K3S_RSS_MB" "$(fmt_or_na "$K3S_CYCLES")" "$(fmt_or_na "$K3S_INSTRUCTIONS")" "$(fmt_or_na "$K3S_IPC")"
    [[ -n "$NODELET_PID" ]] && printf "  %-16s %10s %10s %10s %14s %14s %8s\n" "$AGENT_PATTERN" "${NODELET_CPU_AVG}%" "$NODELET_CPU_SECONDS" "$NODELET_RSS_MB" "$(fmt_or_na "$NODELET_CYCLES")" "$(fmt_or_na "$NODELET_INSTRUCTIONS")" "$(fmt_or_na "$NODELET_IPC")"
    printf "  %-16s %10s %10s %10s %14s %14s %8s\n" "────────────────" "──────────" "──────────" "──────────" "──────────────" "──────────────" "────────"
    printf "  %-16s %10s %10s %10s %14s %14s %8s\n" "COMBINED" "${COMBINED_CPU_AVG}%" "$COMBINED_CPU_SECONDS" "$COMBINED_RSS_MB" "$(fmt_or_na "$COMBINED_CYCLES")" "$(fmt_or_na "$COMBINED_INSTRUCTIONS")" "N/A"
    echo ""
    echo "  test hardware: arch=$CPU_ARCH cores=$CPU_CORES model=\"$CPU_MODEL\""
    echo "  sample window: ${SAMPLE_SECS}s, 1 sample/sec"
    echo "  perf hardware counters (cycles/instructions): $($PERF_OK && echo "available" || echo "unavailable on this runner")"
    echo "  CPU-seconds precision: k3s=$K3S_CPU_SECONDS_SOURCE $AGENT_PATTERN=$NODELET_CPU_SECONDS_SOURCE (perf-task-clock = sub-ms via perf; proc-ticks = ~10ms via /proc, whenever perf's task-clock wasn't available)"
    echo ""
    echo "  NOTE: on stock k3s, \"k3s server\" is the entire stack (apiserver + etcd +"
    echo "  controller-manager + scheduler + the embedded kubelet, when the agent isn't"
    echo "  disabled, all in one OS process) -- k3s does not run kubelet as a separate"
    echo "  process the way a vanilla kubeadm cluster does, so there is no way to"
    echo "  isolate \"kubelet's own number\" from this row at the process level."
} | tee "$OUT_DIR/summary.txt"

machine_block() {
    echo "=== MEASURE ==="
    echo "MEASURE_SAMPLE_SECS=$SAMPLE_SECS"
    echo "MEASURE_PERF_AVAILABLE=$PERF_OK"
    echo "MEASURE_OUT_DIR=$OUT_DIR"
    echo "MEASURE_CPU_ARCH=$CPU_ARCH"
    echo "MEASURE_CPU_CORES=$CPU_CORES"
    echo "MEASURE_CPU_MODEL=$CPU_MODEL"
    echo "MEASURE_K3S_PRESENT=$([[ -n "$K3S_PID" ]] && echo true || echo false)"
    echo "MEASURE_K3S_RSS_MB=$K3S_RSS_MB"
    echo "MEASURE_K3S_CPU_AVG_PCT=$K3S_CPU_AVG"
    echo "MEASURE_K3S_CPU_SECONDS=$K3S_CPU_SECONDS"
    echo "MEASURE_K3S_CPU_SECONDS_SOURCE=$K3S_CPU_SECONDS_SOURCE"
    echo "MEASURE_K3S_CYCLES=$K3S_CYCLES"
    echo "MEASURE_K3S_INSTRUCTIONS=$K3S_INSTRUCTIONS"
    echo "MEASURE_K3S_IPC=$K3S_IPC"
    echo "MEASURE_NODELET_PRESENT=$([[ -n "$NODELET_PID" ]] && echo true || echo false)"
    echo "MEASURE_NODELET_RSS_MB=$NODELET_RSS_MB"
    echo "MEASURE_NODELET_CPU_AVG_PCT=$NODELET_CPU_AVG"
    echo "MEASURE_NODELET_CPU_SECONDS=$NODELET_CPU_SECONDS"
    echo "MEASURE_NODELET_CPU_SECONDS_SOURCE=$NODELET_CPU_SECONDS_SOURCE"
    echo "MEASURE_NODELET_CYCLES=$NODELET_CYCLES"
    echo "MEASURE_NODELET_INSTRUCTIONS=$NODELET_INSTRUCTIONS"
    echo "MEASURE_NODELET_IPC=$NODELET_IPC"
    echo "MEASURE_COMBINED_RSS_MB=$COMBINED_RSS_MB"
    echo "MEASURE_COMBINED_CPU_AVG_PCT=$COMBINED_CPU_AVG"
    echo "MEASURE_COMBINED_CPU_SECONDS=$COMBINED_CPU_SECONDS"
    echo "MEASURE_COMBINED_CYCLES=$COMBINED_CYCLES"
    echo "MEASURE_COMBINED_INSTRUCTIONS=$COMBINED_INSTRUCTIONS"
    echo "MEASURE_K3S_TIMESERIES_CSV=$K3S_TS"
    echo "MEASURE_NODELET_TIMESERIES_CSV=$NODELET_TS"
    echo "=== END MEASURE ==="
}
machine_block | tee -a "$OUT_DIR/summary.txt"
