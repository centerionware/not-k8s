#!/usr/bin/env bash
# profile-nodelet.sh — capture a low-overhead CPU sample of a running nodelet.
#
# The primary artifact is perf.data. The script also captures per-thread CPU
# snapshots, the executable/build identity, nodelet's journal window, and a
# short event-count summary so a profile can be correlated with pod/runtime
# activity. It does not restart or reconfigure nodelet.
#
# Usage:
#   sudo ./deploy/profile-nodelet.sh                 # 30 seconds
#   sudo ./deploy/profile-nodelet.sh --duration 60
#   sudo ./deploy/profile-nodelet.sh --pid 1234 --output /tmp/nodelet-prof
#
# For useful Rust function names, build a symbolized binary separately:
#   cargo build --profile profiling --features cri
#
set -uo pipefail

DURATION="${NODELET_PROFILE_SECONDS:-30}"
PID="${NODELET_PID:-}"
OUT_DIR=""

usage() {
    cat <<'EOF'
profile-nodelet.sh — capture a low-overhead CPU sample of a running nodelet.

The script does not restart or reconfigure nodelet. It writes a timestamped
directory containing perf.data, top-function summaries, per-thread CPU
snapshots, process metadata, and the nodelet journal for the sample window.

Options:
  -d, --duration SECONDS   Sampling duration (default: 30)
  -p, --pid PID            Attach to this nodelet PID
  -o, --output DIRECTORY   Output directory (default: /tmp/not-k8s-nodelet-profile-...)
  -h, --help               Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -d|--duration)
            [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }
            DURATION="$2"
            shift 2
            ;;
        -p|--pid)
            [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }
            PID="$2"
            shift 2
            ;;
        -o|--output)
            [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }
            OUT_DIR="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

[[ "$DURATION" =~ ^[1-9][0-9]*$ ]] || {
    echo "duration must be a positive integer: $DURATION" >&2
    exit 2
}

if [[ -z "$PID" ]]; then
    PID="$(pgrep -xo nodelet 2>/dev/null || true)"
fi
if [[ -z "$PID" ]]; then
    echo "no running nodelet found; pass --pid PID" >&2
    exit 1
fi
if ! [[ "$PID" =~ ^[0-9]+$ ]] || [[ ! -d "/proc/$PID" ]]; then
    echo "nodelet PID is not a live process: $PID" >&2
    exit 1
fi

STAMP="$(date +%Y%m%d-%H%M%S)"
OUT_DIR="${OUT_DIR:-/tmp/not-k8s-nodelet-profile-$STAMP}"
mkdir -p "$OUT_DIR"

START_ISO="$(date --iso-8601=seconds)"
EXE="$(readlink -f "/proc/$PID/exe" 2>/dev/null || true)"
CMDLINE="$(tr '\0' ' ' < "/proc/$PID/cmdline" 2>/dev/null || true)"

echo "==> profiling nodelet PID=$PID for ${DURATION}s"
echo "==> output: $OUT_DIR"

{
    echo "pid=$PID"
    echo "started=$START_ISO"
    echo "duration_seconds=$DURATION"
    echo "executable=$EXE"
    echo "cmdline=$CMDLINE"
    echo
    echo "=== executable ==="
    if command -v file >/dev/null 2>&1; then
        file "$EXE" 2>&1 || true
    else
        echo "file(1) is not installed"
    fi
    echo
    echo "=== cmdline ==="
    cat "/proc/$PID/cmdline" 2>/dev/null | tr '\0' ' '
    echo
    echo
    echo "=== status ==="
    cat "/proc/$PID/status" 2>&1 || true
    echo
    echo "=== limits ==="
    cat "/proc/$PID/limits" 2>&1 || true
    echo
    echo "=== memory map ==="
    cat "/proc/$PID/maps" 2>&1 || true
} > "$OUT_DIR/process.txt"

thread_snapshot() {
    local name="$1"
    {
        echo "=== ps -L ==="
        ps -L -p "$PID" -o pid,tid,psr,pcpu,stat,comm,wchan:32 2>&1 || true
        echo
        echo "=== top -H ==="
        top -H -b -n1 -p "$PID" 2>&1 || true
    } > "$OUT_DIR/$name"
}

thread_snapshot threads-before.txt

capture_with_perf() {
    command -v perf >/dev/null 2>&1 || return 1

    local perf_data="$OUT_DIR/perf.data"
    local timeout_cmd=()
    command -v timeout >/dev/null 2>&1 && timeout_cmd=(timeout "$((DURATION + 15))")

    echo "==> perf available; recording DWARF call stacks"
    rm -f "$perf_data"
    "${timeout_cmd[@]}" perf record -F 99 --call-graph dwarf -p "$PID" -o "$perf_data" -- sleep "$DURATION" \
        > "$OUT_DIR/perf-record.txt" 2>&1
    if [[ ! -s "$perf_data" ]]; then
        echo "DWARF call-graph capture failed; retrying with frame-pointer stacks" >> "$OUT_DIR/perf-record.txt"
        rm -f "$perf_data"
        "${timeout_cmd[@]}" perf record -F 99 -g -p "$PID" -o "$perf_data" -- sleep "$DURATION" \
            >> "$OUT_DIR/perf-record.txt" 2>&1
    fi
    [[ -s "$perf_data" ]] || return 1

    perf report --stdio -i "$perf_data" --sort comm,dso,symbol --percent-limit 0 \
        > "$OUT_DIR/perf-report.txt" 2>&1 || true
    # Leave the self-time report on perf's overhead-descending default order;
    # summarize_perf also sorts numerically before taking the top 20.
    perf report --stdio --no-children -i "$perf_data" --percent-limit 0 \
        > "$OUT_DIR/perf-self-report.txt" 2>&1 || true
    perf script -i "$perf_data" > "$OUT_DIR/perf.script" 2> "$OUT_DIR/perf-script.txt" || true
    if command -v rustfilt >/dev/null 2>&1; then
        rustfilt < "$OUT_DIR/perf.script" > "$OUT_DIR/perf-rustfilt.script" 2> "$OUT_DIR/rustfilt.txt" || true
    fi
    PROFILE_METHOD="perf"
    return 0
}

summarize_perf() {
    local report

    for report in "$OUT_DIR/perf-self-report.txt" "$OUT_DIR/perf-report.txt"; do
        [[ -s "$report" ]] || continue

        # perf's report has one self-time percentage followed by command/object
        # columns and a [.] or [k] marker before the symbol. Keep only sampled
        # function rows, strip those implementation columns, then sort by the
        # numeric percentage so this remains correct if perf's report ordering
        # changes or we fall back to the children-inclusive report.
        awk '
            /^[[:space:]]*[0-9]+(\.[0-9]+)?%/ {
                line = $0
                pct = line
                sub(/^[[:space:]]*/, "", pct)
                sub(/%.*/, "", pct)
                sub(/^.*\[[^]]+\][[:space:]]*/, "", line)
                if (line != "") printf "%6s%%  %s\n", pct, line
            }
        ' "$report" | sort -nr | head -20 > "$OUT_DIR/top-functions.txt"
        [[ -s "$OUT_DIR/top-functions.txt" ]] && return 0
    done

    return 1
}

summarize_strace() {
    [[ -s "$OUT_DIR/strace-summary.txt" ]] || return 1
    # Keep the syscall table compact for the console; the complete table stays
    # in strace-summary.txt.
    awk 'NR == 1 || /^[[:space:]]*[0-9]/ { print }' "$OUT_DIR/strace-summary.txt" \
        | head -15 > "$OUT_DIR/top-syscalls.txt"
    [[ -s "$OUT_DIR/top-syscalls.txt" ]]
}

capture_with_strace() {
    command -v strace >/dev/null 2>&1 || return 1
    echo "==> perf unavailable or blocked; collecting syscall fallback with strace"
    local timeout_cmd=()
    command -v timeout >/dev/null 2>&1 && timeout_cmd=(timeout "$((DURATION + 15))")
    "${timeout_cmd[@]}" strace -f -c -p "$PID" -o "$OUT_DIR/strace-summary.txt" \
        > "$OUT_DIR/strace.txt" 2>&1 || return 1
    [[ -s "$OUT_DIR/strace-summary.txt" ]] || return 1
    PROFILE_METHOD="strace-fallback"
    return 0
}

PROFILE_METHOD="none"
if ! capture_with_perf; then
    if ! capture_with_strace; then
        echo "WARNING: neither perf nor strace is available/usable; sleeping for the sample window." >&2
        PROFILE_METHOD="none"
        sleep "$DURATION"
    fi
fi

END_ISO="$(date --iso-8601=seconds)"
thread_snapshot threads-after.txt

if [[ "$PROFILE_METHOD" == "perf" ]]; then
    summarize_perf || true
elif [[ "$PROFILE_METHOD" == "strace-fallback" ]]; then
    summarize_strace || true
fi

if command -v journalctl >/dev/null 2>&1; then
    journalctl -u nodelet --since "$START_ISO" --until "$END_ISO" --no-pager -o short-iso-precise \
        > "$OUT_DIR/nodelet-journal.txt" 2>&1 || true
fi

count_log() {
    local pattern="$1"
    [[ -f "$OUT_DIR/nodelet-journal.txt" ]] || { echo 0; return; }
    awk -v pattern="$pattern" '$0 ~ pattern { count++ } END { print count + 0 }' \
        "$OUT_DIR/nodelet-journal.txt"
}

{
    echo "nodelet CPU profile"
    echo "=================="
    echo "pid: $PID"
    echo "started: $START_ISO"
    echo "ended: $END_ISO"
    echo "duration_seconds: $DURATION"
    echo "profile_method: $PROFILE_METHOD"
    echo "executable: $EXE"
    echo "cmdline: $CMDLINE"
    echo
    echo "event counts from nodelet journal:"
    echo "  watch/runtime ensures: $(count_log 'ensured')"
    echo "  runtime status updates: $(count_log 'status updated')"
    echo "  skipped unchanged patches: $(count_log 'skipped unchanged pod status patch')"
    echo "  CRI events: $(count_log 'CRI container event')"
    echo "  containerd task events: $(count_log 'containerd task event')"
    echo "  warnings/errors: $(count_log 'WARN|ERROR|failed|error=')"
    echo
    echo "files:"
    find "$OUT_DIR" -maxdepth 1 -type f -printf '  %f\n' | sort
} > "$OUT_DIR/SUMMARY.txt"

echo "==> profile complete"
cat "$OUT_DIR/SUMMARY.txt"
echo
echo "===== hottest sampled functions (self time) ====="
if [[ -s "$OUT_DIR/top-functions.txt" ]]; then
    cat "$OUT_DIR/top-functions.txt"
elif [[ -s "$OUT_DIR/top-syscalls.txt" ]]; then
    echo "perf was unavailable; top syscalls from strace fallback:"
    cat "$OUT_DIR/top-syscalls.txt"
else
    echo "No function/syscall summary was produced. Check perf-record.txt or strace.txt."
fi
echo
echo "===== busiest threads after capture ====="
ps -L -p "$PID" --sort=-pcpu -o tid,psr,pcpu,stat,comm,wchan:32 2>&1 | head -15 || true
echo "==> send this directory (especially perf-report.txt, perf-rustfilt.script, threads-*, and nodelet-journal.txt)"
