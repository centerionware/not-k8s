#!/usr/bin/env bash
# measure.sh — Measure idle CPU%/CPU-seconds/RSS *over time* (1-second
# samples, not just a single before/after delta), and — where the host's
# perf_event access allows it — real hardware cycles/instructions, for every
# process this stack runs.
#
# ── One row per component, not one variable pair per component ──────────────
#
# This script used to know about exactly two processes, "k3s server" and "the
# node agent", as two hand-maintained sets of K3S_*/NODELET_* variables
# threaded through sampling, perf, summarising and reporting. That shape was
# already wrong once nodeproxy was split out of nodelet (its cost silently
# stopped being counted anywhere — see .github/workflows/profiling.yml's own
# note on why the published comparison runs --proxy=none), and wrong again
# once nodestore replaced kine (the work moved *out* of the k3s process into
# one nothing was watching, which flatters the k3s number for free).
#
# So components live in the table below, the same way deploy/lib/components.sh
# holds the deploy side's list. **Adding a component — `nodeapiserver`,
# `nodescheduler`, whatever replaces the next piece of the control plane — is
# one row here.** Nothing else in this file names a component.
#
# A row whose process is not running is reported as absent and skipped, so the
# same table covers every deployment shape: with or without nodeproxy
# (--proxy=none), with kine or with nodestore (--datastore=nodestore), and,
# later, with k3s's apiserver/scheduler or with ours.
#
# ── What the k3s row actually contains ─────────────────────────────────────
#
# k3s is one OS process holding whatever has not been replaced yet: apiserver,
# controller-manager, scheduler, kine+SQLite unless a datastore row below is
# running, and the embedded kubelet unless the agent is disabled (this repo's
# bootstrap always passes --disable-agent). There is no way to isolate those
# at the process level, so the k3s row is a *remainder*, and it shrinks as
# components move out of it. The summary says so rather than letting the
# number be read as "the apiserver's cost".
#
# Outputs (written under the out-dir, default a fresh mktemp -d):
#   <slot>-timeseries.csv   second,rss_kb,cpu_pct — the real per-second data
#   summary.txt             human-readable table + MEASURE_* machine block
#     (cycles/instructions/IPC in the summary are a single whole-window
#     aggregate from perf, not a per-second series — see the perf section
#     below for why)
#
# The `k3s-timeseries.csv` and `nodelet-timeseries.csv` names, and the
# MEASURE_K3S_*/MEASURE_NODELET_* keys, are load-bearing: profiling.yml's
# report job and deploy/lib/render-profiling-charts.py read exactly those.
# New components add new names alongside them; these two do not get renamed.
#
# Usage:
#   ./deploy/measure.sh                        # 30s sample window
#   ./deploy/measure.sh 120                    # 120s window
#   ./deploy/measure.sh 120 /tmp/measure-out   # explicit output directory
#   ./deploy/measure.sh 120 /tmp/out kubelet   # measure a different node agent
#
set -uo pipefail

SAMPLE_SECS="${1:-30}"
OUT_DIR="${2:-}"
# The node-agent side of the comparison is nodelet by default, but this same
# script drives the upstream-kubelet.sh profiling leg (deploy/lib/upstream-
# kubelet.sh) — pass a different process-match pattern (e.g. "kubelet") as $3
# to measure that agent instead. The slot keeps its `nodelet` identity in the
# output either way: this file's job is "the node agent slot", and the
# published report compares the two legs by putting a different process in the
# same slot.
AGENT_PATTERN="${3:-nodelet}"
[[ -n "$OUT_DIR" ]] || OUT_DIR="$(mktemp -d /tmp/not-k8s-measure.XXXXXX)"
mkdir -p "$OUT_DIR"

# ── The component table ─────────────────────────────────────────────────────
#
# Row format (pipe-separated, no spaces around the pipes):
#
#   <slot>|<match pattern>|<label>|<what it is>
#
#   slot     identifies this component everywhere in the output: its CSV is
#            <slot>-timeseries.csv and its machine-block keys are
#            MEASURE_<SLOT>_*. Stable across releases — a report compares
#            runs by these names, so renaming one breaks history.
#   pattern  what to look for. Matched against the executable name first and
#            the full command line second — see find_pid() for why that order
#            matters.
#   label    what the human-readable table calls it.
#   what     one line for the summary's legend, so a reader who has never
#            seen this stack knows what the row is.
#
# Order is the order rows print in: control plane first, then the datastore,
# then the node components.
MEASURE_COMPONENTS=(
    "k3s|k3s server|k3s server|everything not yet replaced: apiserver, controller-manager, scheduler, and kine+SQLite unless a datastore row below is running"
    "nodestore|nodestore|nodestore|the datastore — etcd v3 over SQLite, replacing kine"
    "nodelet|$AGENT_PATTERN|$AGENT_PATTERN|the node agent, replacing kubelet"
    "nodeproxy|nodeproxy|nodeproxy|Service/ClusterIP/NodePort routing, replacing kube-proxy"
    # Not written yet. Rows cost nothing when the process is absent, and
    # having them here means the day they exist they are measured by the
    # profiling that already runs, rather than the day someone remembers.
    "nodeapiserver|nodeapiserver|nodeapiserver|the API server, replacing kube-apiserver"
    "nodescheduler|nodescheduler|nodescheduler|the scheduler, replacing kube-scheduler"
    "nodecontroller|nodecontroller|nodecontroller|the controller manager — node lifecycle, ServiceAccount/CSR, GC and friends — replacing kube-controller-manager"
    # The upstream components this project replaces, run as real standalone
    # processes by deploy/lib/upstream-kubelet.sh,
    # deploy/lib/upstream-kube-proxy.sh, deploy/lib/upstream-kube-scheduler.sh,
    # deploy/lib/upstream-kube-apiserver.sh and
    # deploy/lib/upstream-kube-controller-manager.sh. Present only on the
    # upstream leg of a comparison, absent on ours — which is what lets one
    # table measure both legs without being told which leg it is on.
    #
    # kube-apiserver/kube-controller-manager are a special case among these
    # five: when both are running standalone, k3s itself is stopped
    # entirely (see upstream-kube-apiserver.sh's own header — two apiservers
    # cannot both write to the same datastore), so the k3s row above is
    # correctly absent rather than double-counting a remainder that no
    # longer exists.
    #
    # kube-apiserver's row comes before kubelet's, deliberately, and not
    # alphabetically: find_pid() tries an exact executable-name match
    # (pgrep -x) before falling back to a full-cmdline substring search
    # (pgrep -f), and rows run in table order. kube-apiserver's own flags
    # (--kubelet-client-certificate, --kubelet-certificate-authority, ...)
    # contain the literal substring "kubelet" — confirmed live: with
    # kubelet's row first, its -f fallback matched kube-apiserver's PID
    # before kube-apiserver's own -x match ever got a turn, so
    # kube-apiserver was reported absent and "kubelet" silently measured
    # the apiserver instead. Giving kube-apiserver's exact match first claim
    # on its own PID (CLAIMED_PIDS) makes kubelet's later fallback search
    # correctly find nothing when no standalone kubelet is actually running.
    "kube-apiserver|kube-apiserver|kube-apiserver|upstream kube-apiserver, the API server nodeapiserver will replace"
    "kube-controller-manager|kube-controller-manager|kube-controller-manager|upstream kube-controller-manager (node lifecycle, ServiceAccount/CSR controllers) — nothing in not-k8s replaces this yet"
    "kubelet|kubelet|kubelet|upstream kubelet, the node agent nodelet replaces"
    "kube-proxy|kube-proxy|kube-proxy|upstream kube-proxy, the Service routing nodeproxy replaces"
    "kube-scheduler|kube-scheduler|kube-scheduler|upstream kube-scheduler, the pod placement nodescheduler replaces"
    # Not ours, and measured precisely because they are not.
    #
    # Stock k3s runs flannel *inside* the k3s process and brings its own
    # containerd; this stack runs flanneld as its own service and uses the
    # host's containerd. Counting only the not-k8s binaries would put
    # flannel's cost inside k3s's row on one side and nowhere at all on the
    # other, which makes the comparison flatter this project for free — the
    # exact failure this table exists to stop. They are rows, so both sides
    # are whole-stack numbers.
    "containerd|containerd|containerd|the container runtime (k3s bundles its own; this stack uses the host's)"
    "flanneld|flanneld|flanneld|the CNI overlay daemon (a separate service here, in-process on stock k3s)"
)

# ── Process discovery ───────────────────────────────────────────────────────

# Our own process tree, so a pattern can never match the thing doing the
# measuring. This is not hypothetical: every one of these patterns appears in
# this script's own command line and in the shell that launched it, and a
# full-command-line match would happily return that shell — a process with a
# tiny RSS and no CPU, which reports as a beautifully efficient component.
_self_tree() {
    local pid=$$ depth=0
    while [[ "$pid" -gt 1 && "$depth" -lt 12 ]]; do
        echo "$pid"
        pid="$(awk '{ print $4 }' "/proc/$pid/stat" 2>/dev/null)" || break
        [[ -n "$pid" ]] || break
        depth=$(( depth + 1 ))
    done
}
SELF_TREE="$(_self_tree | tr '\n' ' ')"

# The oldest process matching a pattern, excluding ourselves.
#
# Executable name first (`pgrep -x`), full command line second (`pgrep -f`):
# a bare name like "nodelet" matches the real binary exactly, while the
# command-line fallback is what finds "k3s server" (two words, only ever a
# command line) and a path-qualified pattern like "/usr/local/bin/kubelet".
# Doing it in the other order is how a wrapper script — run-nodelet.sh, or the
# shell that invoked this one — wins over the binary it started, and the
# measurement then reports the wrapper's near-zero footprint as the
# component's.
find_pid() {
    local pattern="$1" candidates pid
    for candidates in "$(pgrep -x "$pattern" 2>/dev/null)" "$(pgrep -f "$pattern" 2>/dev/null)"; do
        for pid in $candidates; do
            [[ " $SELF_TREE " == *" $pid "* ]] && continue
            echo "$pid"
            return 0
        done
    done
    return 0
}

get_rss_kb() {
    awk '/^VmRSS:/ { print $2 }' "/proc/$1/status" 2>/dev/null || echo "0"
}
get_cpu_ticks() {
    local pid="$1" stat after_comm
    stat="$(cat "/proc/$pid/stat" 2>/dev/null)" || { echo "0"; return; }
    # Everything after the comm field, because comm can itself contain spaces
    # and parentheses; utime and stime are then fields 12 and 13.
    after_comm="${stat##*) }"
    echo $(( $(echo "$after_comm" | awk '{print $12}') + $(echo "$after_comm" | awk '{print $13}') ))
}

declare -A SLOT_PATTERN SLOT_LABEL SLOT_WHAT SLOT_PID SLOT_CSV
declare -A SLOT_PREV_TICKS SLOT_TICKS_TOTAL SLOT_RSS_PEAK
declare -A SLOT_PERF_RAW SLOT_PERF_BGPID
declare -A SLOT_CPU_AVG SLOT_RSS_MB SLOT_CPU_SECONDS SLOT_CPU_SECONDS_SOURCE
declare -A SLOT_CYCLES SLOT_INSTRUCTIONS SLOT_IPC SLOT_TASK_CLOCK
SLOTS=()
PRESENT=()

CLAIMED_PIDS=" "
for row in "${MEASURE_COMPONENTS[@]}"; do
    IFS='|' read -r slot pattern label what <<<"$row"
    SLOTS+=("$slot")
    SLOT_PATTERN[$slot]="$pattern"
    SLOT_LABEL[$slot]="$label"
    SLOT_WHAT[$slot]="$what"
    SLOT_CSV[$slot]="$OUT_DIR/${slot}-timeseries.csv"
    pid="$(find_pid "$pattern")"
    # One process is counted once. The node-agent slot's pattern is
    # caller-overridable ($3), so pointing it at kubelet makes it match the
    # same process the `kubelet` row below matches — and COMBINED would then
    # count that process twice, inflating the leg this comparison is supposed
    # to be fair to. First row to claim a PID keeps it.
    if [[ -n "$pid" && "$CLAIMED_PIDS" == *" $pid "* ]]; then
        pid=""
    fi
    SLOT_PID[$slot]="$pid"
    if [[ -n "$pid" ]]; then
        CLAIMED_PIDS="$CLAIMED_PIDS$pid "
        PRESENT+=("$slot")
    fi
done

if [[ "${#PRESENT[@]}" -eq 0 ]]; then
    echo "ERROR: none of the components in the table are running." >&2
    echo "       Start the control plane and the node components first." >&2
    exit 1
fi

echo "==> Process discovery:"
for slot in "${SLOTS[@]}"; do
    if [[ -n "${SLOT_PID[$slot]}" ]]; then
        printf '    %-14s PID %s (matched %s)\n' "$slot" "${SLOT_PID[$slot]}" "${SLOT_PATTERN[$slot]}"
    else
        printf '    %-14s NOT RUNNING\n' "$slot"
    fi
done
echo "    output directory: $OUT_DIR"
echo ""

CLK_TCK="$(getconf CLK_TCK)"

# ── Test hardware, for context on every report this feeds ───────────────────
# Results are only meaningful relative to the specific hardware they ran on
# (GitHub-hosted runners aren't a fixed spec -- Azure picks whatever's
# available in a pool at dispatch time), so record it rather than leaving a
# reader to assume/guess. x86 cpuinfo has "model name"; ARM's doesn't, usually
# just "Model" (the board) or numeric "CPU part" -- tried in that order,
# falling back to "unknown" rather than a guess.
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
if $PERF_INSTALLED; then
    if $PERF_OK; then
        echo "==> perf available with real hardware counters; sampling cycles/instructions/task-clock (whole-window aggregate), concurrently with the per-second RSS/CPU loop below"
    else
        echo "==> perf is installed but hardware performance counters aren't reachable on this host"
        echo "    (confirmed common on virtualized cloud runners -- the hypervisor doesn't expose the"
        echo "    PMU to the guest at all, root or not); still sampling task-clock, a software event"
        echo "    that gives sub-millisecond-precision CPU time regardless."
    fi
    for slot in "${PRESENT[@]}"; do
        SLOT_PERF_RAW[$slot]="$OUT_DIR/.${slot}-perf-raw.txt"
        perf stat -e cycles,instructions,task-clock -p "${SLOT_PID[$slot]}" \
            -o "${SLOT_PERF_RAW[$slot]}" -- sleep "$SAMPLE_SECS" 2>/dev/null &
        SLOT_PERF_BGPID[$slot]=$!
    done
else
    echo "==> perf not installed; cycles/instructions/task-clock will be omitted, CPU-seconds (from /proc, ~10ms"
    echo "    resolution) and RSS remain the primary metrics."
fi
echo ""
echo "==> sampling RSS + CPU% every 1s for ${SAMPLE_SECS}s across ${#PRESENT[@]} component(s)..."

for slot in "${PRESENT[@]}"; do
    echo "second,rss_kb,cpu_pct" > "${SLOT_CSV[$slot]}"
    SLOT_PREV_TICKS[$slot]="$(get_cpu_ticks "${SLOT_PID[$slot]}")"
    SLOT_TICKS_TOTAL[$slot]=0
    SLOT_RSS_PEAK[$slot]=0
done

for (( sec=1; sec<=SAMPLE_SECS; sec++ )); do
    sleep 1
    for slot in "${PRESENT[@]}"; do
        pid="${SLOT_PID[$slot]}"
        [[ -d "/proc/$pid" ]] || continue
        rss="$(get_rss_kb "$pid")"
        ticks="$(get_cpu_ticks "$pid")"
        delta=$(( ticks - SLOT_PREV_TICKS[$slot] ))
        (( delta < 0 )) && delta=0
        SLOT_PREV_TICKS[$slot]="$ticks"
        SLOT_TICKS_TOTAL[$slot]=$(( SLOT_TICKS_TOTAL[$slot] + delta ))
        (( rss > SLOT_RSS_PEAK[$slot] )) && SLOT_RSS_PEAK[$slot]="$rss"
        pct="$(awk -v d="$delta" -v c="$CLK_TCK" 'BEGIN { printf "%.2f", (d / c) * 100 }')"
        echo "$sec,$rss,$pct" >> "${SLOT_CSV[$slot]}"
    done
done

for slot in "${PRESENT[@]}"; do
    [[ -n "${SLOT_PERF_BGPID[$slot]:-}" ]] && wait "${SLOT_PERF_BGPID[$slot]}" 2>/dev/null
done

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

# task-clock is a software event (always available, even when hardware
# cycles/instructions aren't -- see the perf section above), with real
# decimal precision unlike parse_perf_value's integer-only cycles/
# instructions parsing. Round 124 (found live in a published report: a
# nodelet reading of 88709 "CPU-seconds" inside a 120-second window --
# physically impossible): perf's own unit label for this event isn't a
# fixed "msec" the way this function first assumed -- it varies (msec/
# usec/nsec) by perf version/config, and blindly dividing by a hardcoded
# 1000 silently produced a 1000x-off result the one time it wasn't
# actually msec. Parses the real unit word from perf's own output instead
# of assuming one, and returns real SECONDS directly (not milliseconds)
# so there's no second conversion step downstream to get wrong too.
parse_perf_task_clock_seconds() {
    local file="$1"
    [[ -s "$file" ]] || { echo ""; return; }
    awk '
        /task-clock/ {
            val = $1
            unit = $2
            gsub(",", "", val)
            if (val !~ /^[0-9]+(\.[0-9]+)?$/) next
            if (unit == "msec") { printf "%.9f", val / 1000; exit }
            if (unit == "usec") { printf "%.9f", val / 1000000; exit }
            if (unit == "nsec") { printf "%.9f", val / 1000000000; exit }
            if (unit == "sec")  { printf "%.9f", val; exit }
            # Unrecognized unit label -- do not guess at a conversion
            # factor; leave this run without a perf-precision reading
            # rather than risk another silent unit-scale bug.
        }
    ' "$file"
}

# ── Summary stats derived from the time series ───────────────────────────────

summarize_cpu_avg() {
    local csv="$1"
    [[ -s "$csv" ]] || { echo "0.00"; return; }
    awk -F, 'NR>1 { n++; cpu_sum += $3 } END { printf "%.2f", (n>0 ? cpu_sum/n : 0) }' "$csv"
}

# CPU-seconds: prefer perf's task-clock (sub-millisecond precision,
# already normalized to real seconds above) when it actually reported
# one; fall back to the /proc-tick delta (~10ms resolution, CLK_TCK-
# limited) otherwise. Track which source won so the report can say so
# honestly rather than implying uniform precision. Sanity-clamped against
# the sample window itself (per core) -- a value that large is proof of a
# unit-conversion bug, not a real reading, and reporting it as "N/A"
# beats silently republishing something physically impossible again.
cpu_seconds_from_ticks_or_task_clock() {
    local ticks_total="$1" task_clock_seconds="$2"
    local candidate
    if [[ -n "$task_clock_seconds" ]]; then
        candidate="$task_clock_seconds"
    else
        candidate="$(awk -v t="$ticks_total" -v c="$CLK_TCK" 'BEGIN { printf "%.3f", t / c }')"
    fi
    awk -v v="$candidate" -v s="$SAMPLE_SECS" -v cores="$CPU_CORES" 'BEGIN {
        max = s * ((cores + 0 > 0) ? cores : 1) * 1.05
        if (v + 0 > max) { print ""; exit }
        printf "%.6f", v
    }'
}

calc_ipc() {
    [[ -n "$1" && -n "$2" && "$2" != "0" ]] || { echo ""; return; }
    awk -v i="$1" -v c="$2" 'BEGIN { printf "%.3f", i / c }'
}

for slot in "${PRESENT[@]}"; do
    raw="${SLOT_PERF_RAW[$slot]:-}"
    SLOT_CYCLES[$slot]="$(parse_perf_value "$raw" 'cycles')"
    SLOT_INSTRUCTIONS[$slot]="$(parse_perf_value "$raw" 'instructions')"
    SLOT_TASK_CLOCK[$slot]="$(parse_perf_task_clock_seconds "$raw")"
    [[ -n "$raw" ]] && rm -f "$raw"

    SLOT_CPU_AVG[$slot]="$(summarize_cpu_avg "${SLOT_CSV[$slot]}")"
    SLOT_RSS_MB[$slot]="$(awk -v kb="${SLOT_RSS_PEAK[$slot]}" 'BEGIN { printf "%.1f", kb / 1024 }')"
    SLOT_CPU_SECONDS[$slot]="$(cpu_seconds_from_ticks_or_task_clock \
        "${SLOT_TICKS_TOTAL[$slot]}" "${SLOT_TASK_CLOCK[$slot]}")"
    SLOT_CPU_SECONDS_SOURCE[$slot]="$([[ -n "${SLOT_TASK_CLOCK[$slot]}" ]] && echo "perf-task-clock" || echo "proc-ticks")"
    SLOT_IPC[$slot]="$(calc_ipc "${SLOT_INSTRUCTIONS[$slot]}" "${SLOT_CYCLES[$slot]}")"
done

# COMBINED is every present component summed — the whole stack's real cost on
# this node, which is the number that actually matters to anyone deciding
# whether to run it. With only k3s and a node agent present (what
# profiling.yml's published comparison runs) this is exactly what it has
# always been.
COMBINED_RSS_MB=0; COMBINED_CPU_AVG=0; COMBINED_CPU_SECONDS=0
COMBINED_CYCLES=""; COMBINED_INSTRUCTIONS=""
sum_or_blank() {
    [[ -z "$1" && -z "$2" ]] && { echo ""; return; }
    awk -v a="${1:-0}" -v b="${2:-0}" 'BEGIN { printf "%.0f", a + b }'
}
for slot in "${PRESENT[@]}"; do
    COMBINED_RSS_MB="$(awk -v a="$COMBINED_RSS_MB" -v b="${SLOT_RSS_MB[$slot]}" 'BEGIN { printf "%.1f", a + b }')"
    COMBINED_CPU_AVG="$(awk -v a="$COMBINED_CPU_AVG" -v b="${SLOT_CPU_AVG[$slot]}" 'BEGIN { printf "%.2f", a + b }')"
    COMBINED_CPU_SECONDS="$(awk -v a="$COMBINED_CPU_SECONDS" -v b="${SLOT_CPU_SECONDS[$slot]:-0}" 'BEGIN { printf "%.3f", a + b }')"
    COMBINED_CYCLES="$(sum_or_blank "$COMBINED_CYCLES" "${SLOT_CYCLES[$slot]}")"
    COMBINED_INSTRUCTIONS="$(sum_or_blank "$COMBINED_INSTRUCTIONS" "${SLOT_INSTRUCTIONS[$slot]}")"
done

# ── Human-readable + machine-readable summary ───────────────────────────────

fmt_or_na() { [[ -n "$1" ]] && echo "$1" || echo "N/A"; }

row_fmt="  %-16s %10s %10s %10s %14s %14s %8s\n"
rule() { printf "$row_fmt" "────────────────" "──────────" "──────────" "──────────" "──────────────" "──────────────" "────────"; }
{
    printf "$row_fmt" "PROCESS" "avg CPU%" "CPU-sec" "RSS (MB)" "cycles" "instructions" "IPC"
    rule
    for slot in "${PRESENT[@]}"; do
        printf "$row_fmt" "${SLOT_LABEL[$slot]}" "${SLOT_CPU_AVG[$slot]}%" \
            "$(fmt_or_na "${SLOT_CPU_SECONDS[$slot]}")" "${SLOT_RSS_MB[$slot]}" \
            "$(fmt_or_na "${SLOT_CYCLES[$slot]}")" "$(fmt_or_na "${SLOT_INSTRUCTIONS[$slot]}")" \
            "$(fmt_or_na "${SLOT_IPC[$slot]}")"
    done
    rule
    printf "$row_fmt" "COMBINED" "${COMBINED_CPU_AVG}%" "$COMBINED_CPU_SECONDS" "$COMBINED_RSS_MB" \
        "$(fmt_or_na "$COMBINED_CYCLES")" "$(fmt_or_na "$COMBINED_INSTRUCTIONS")" "N/A"
    echo ""
    echo "  what each row is:"
    for slot in "${PRESENT[@]}"; do
        printf "    %-14s %s\n" "${SLOT_LABEL[$slot]}" "${SLOT_WHAT[$slot]}"
    done
    absent=""
    for slot in "${SLOTS[@]}"; do
        [[ -z "${SLOT_PID[$slot]}" ]] && absent="$absent $slot"
    done
    [[ -n "$absent" ]] && echo "  not running on this host (not measured):$absent"
    echo ""
    echo "  test hardware: arch=$CPU_ARCH cores=$CPU_CORES model=\"$CPU_MODEL\""
    echo "  sample window: ${SAMPLE_SECS}s, 1 sample/sec"
    echo "  perf hardware counters (cycles/instructions): $($PERF_OK && echo "available" || echo "unavailable on this host")"
    printf "  CPU-seconds precision:"
    for slot in "${PRESENT[@]}"; do printf " %s=%s" "$slot" "${SLOT_CPU_SECONDS_SOURCE[$slot]}"; done
    echo " (perf-task-clock = sub-ms via perf; proc-ticks = ~10ms via /proc, whenever perf's task-clock wasn't available)"
    echo ""
    echo "  NOTE: the \"k3s server\" row is one OS process holding everything not yet"
    echo "  replaced — apiserver + controller-manager + scheduler, plus kine/SQLite"
    echo "  whenever nodestore is not the datastore, plus the embedded kubelet whenever"
    echo "  the agent isn't disabled. Those cannot be isolated at the process level, so"
    echo "  read that row as a remainder that shrinks as components move out of it, not"
    echo "  as any one component's cost."
} | tee "$OUT_DIR/summary.txt"

machine_block() {
    echo "=== MEASURE ==="
    echo "MEASURE_SAMPLE_SECS=$SAMPLE_SECS"
    echo "MEASURE_PERF_AVAILABLE=$PERF_OK"
    echo "MEASURE_OUT_DIR=$OUT_DIR"
    echo "MEASURE_CPU_ARCH=$CPU_ARCH"
    echo "MEASURE_CPU_CORES=$CPU_CORES"
    echo "MEASURE_CPU_MODEL=$CPU_MODEL"
    # Every slot in the table gets keys, present or not, so a consumer can
    # tell "measured as zero" from "was not running" without guessing.
    for slot in "${SLOTS[@]}"; do
        key="$(echo "$slot" | tr '[:lower:]-' '[:upper:]_')"
        if [[ -z "${SLOT_PID[$slot]}" ]]; then
            echo "MEASURE_${key}_PRESENT=false"
            continue
        fi
        echo "MEASURE_${key}_PRESENT=true"
        echo "MEASURE_${key}_RSS_MB=${SLOT_RSS_MB[$slot]}"
        echo "MEASURE_${key}_CPU_AVG_PCT=${SLOT_CPU_AVG[$slot]}"
        echo "MEASURE_${key}_CPU_SECONDS=${SLOT_CPU_SECONDS[$slot]}"
        echo "MEASURE_${key}_CPU_SECONDS_SOURCE=${SLOT_CPU_SECONDS_SOURCE[$slot]}"
        echo "MEASURE_${key}_CYCLES=${SLOT_CYCLES[$slot]}"
        echo "MEASURE_${key}_INSTRUCTIONS=${SLOT_INSTRUCTIONS[$slot]}"
        echo "MEASURE_${key}_IPC=${SLOT_IPC[$slot]}"
        echo "MEASURE_${key}_TIMESERIES_CSV=${SLOT_CSV[$slot]}"
    done
    echo "MEASURE_COMBINED_RSS_MB=$COMBINED_RSS_MB"
    echo "MEASURE_COMBINED_CPU_AVG_PCT=$COMBINED_CPU_AVG"
    echo "MEASURE_COMBINED_CPU_SECONDS=$COMBINED_CPU_SECONDS"
    echo "MEASURE_COMBINED_CYCLES=$COMBINED_CYCLES"
    echo "MEASURE_COMBINED_INSTRUCTIONS=$COMBINED_INSTRUCTIONS"
    echo "=== END MEASURE ==="
}
machine_block | tee -a "$OUT_DIR/summary.txt"
