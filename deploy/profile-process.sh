#!/usr/bin/env bash
# profile-process.sh — capture a low-overhead CPU sample of any running
# not-k8s component and render a REAL flame graph SVG from it, not just a
# textual top-functions list.
#
# Generalized from the nodelet-only deploy/profile-nodelet.sh (still present,
# untouched, for anyone with existing muscle memory around it) — this one
# takes the process to attach to as a parameter instead of assuming nodelet,
# so it works for nodescheduler, nodecontroller, nodestore, or anything else
# in this stack without a new near-duplicate script per component.
#
# The primary artifacts are perf.data and flamegraph.svg. The script also
# captures per-thread CPU snapshots, the executable/build identity, and (if
# --journal-unit is given) that unit's journal window for the sample period.
# It does not restart or reconfigure the profiled process.
#
# Usage:
#   sudo ./deploy/profile-process.sh --pattern nodecontroller --journal-unit nodecontroller
#   sudo ./deploy/profile-process.sh --pid 1234 --duration 60 --label nodecontroller
#   sudo ./deploy/profile-process.sh --pattern nodescheduler --output /tmp/ns-prof
#
# For a flame graph with real Rust function names (not addresses), profile a
# binary built with debug info — a debug build, or `cargo build --profile
# profiling`, not a stripped release binary.
set -uo pipefail

DURATION="${PROFILE_SECONDS:-30}"
PID=""
PATTERN=""
LABEL=""
JOURNAL_UNIT=""
OUT_DIR=""
STOP_FILE=""
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FG_DIR="$REPO_ROOT/.bootstrap/flamegraph-tools"
FG_DIR="${FLAMEGRAPH_DIR:-$FG_DIR}"
FG_REVISION=41fee1f99f9276008b7cd112fca19dc3ea84ac32

usage() {
    cat <<'EOF'
profile-process.sh — capture a real flame graph of any running not-k8s
component (or any process at all — this has nothing not-k8s-specific in it
beyond the default output-directory prefix).

Options:
  -p, --pid PID             Attach to this PID directly
  -P, --pattern PATTERN     Find the PID via `pgrep -x PATTERN` (exact
                             executable-name match, same rule measure.sh
                             uses and for the same reason: a full-cmdline
                             match can find the wrong process)
  -l, --label NAME          Name used in output filenames/titles (default:
                             PATTERN, or "process" if only --pid was given)
  -j, --journal-unit UNIT   Also capture this systemd unit's journal for the
                             sample window, for correlating spikes with logs
  -d, --duration SECONDS    Sampling duration (default: 30) — a hard cap
                             when --stop-file is given too (see below),
                             the actual sample length otherwise.
  -s, --stop-file FILE      Stop recording as soon as this file appears,
                             instead of always running the full --duration
                             — same MEASURE_STOP_FILE convention
                             deploy/measure.sh already uses, for a caller
                             bracketing perf around real work of unknown
                             length (e.g. "until this pod is deleted")
                             rather than a pre-guessed fixed window.
                             --duration still applies as a safety cap so a
                             caller that never creates the file can't hang
                             this forever.
  -o, --output DIRECTORY    Output directory (default:
                             /tmp/not-k8s-profile-<label>-<timestamp>)
  -h, --help                Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -p|--pid) [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }; PID="$2"; shift 2 ;;
        -P|--pattern) [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }; PATTERN="$2"; shift 2 ;;
        -l|--label) [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }; LABEL="$2"; shift 2 ;;
        -j|--journal-unit) [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }; JOURNAL_UNIT="$2"; shift 2 ;;
        -d|--duration) [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }; DURATION="$2"; shift 2 ;;
        -s|--stop-file) [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }; STOP_FILE="$2"; shift 2 ;;
        -o|--output) [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }; OUT_DIR="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

[[ "$DURATION" =~ ^[1-9][0-9]*$ ]] || { echo "duration must be a positive integer: $DURATION" >&2; exit 2; }

if [[ -z "$PID" ]]; then
    [[ -n "$PATTERN" ]] || { echo "need --pid or --pattern" >&2; usage >&2; exit 2; }
    PID="$(pgrep -xo "$PATTERN" 2>/dev/null || true)"
fi
[[ -n "$PID" ]] || { echo "no matching process found (pattern: ${PATTERN:-none given}); pass --pid PID" >&2; exit 1; }
[[ "$PID" =~ ^[0-9]+$ && -d "/proc/$PID" ]] || { echo "PID is not a live process: $PID" >&2; exit 1; }

LABEL="${LABEL:-${PATTERN:-process}}"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT_DIR="${OUT_DIR:-/tmp/not-k8s-profile-$LABEL-$STAMP}"
mkdir -p "$OUT_DIR"

START_ISO="$(date --iso-8601=seconds)"
EXE="$(readlink -f "/proc/$PID/exe" 2>/dev/null || true)"
CMDLINE="$(tr '\0' ' ' < "/proc/$PID/cmdline" 2>/dev/null || true)"

echo "==> profiling '$LABEL' PID=$PID for ${DURATION}s"
echo "==> executable: $EXE"
echo "==> output: $OUT_DIR"

{
    echo "label=$LABEL"; echo "pid=$PID"; echo "started=$START_ISO"
    echo "duration_seconds=$DURATION"; echo "executable=$EXE"; echo "cmdline=$CMDLINE"; echo
    echo "=== executable ==="; command -v file >/dev/null 2>&1 && file "$EXE" 2>&1 || echo "file(1) not installed"
    echo; echo "=== status ==="; cat "/proc/$PID/status" 2>&1 || true
} > "$OUT_DIR/process.txt"

thread_snapshot() {
    { echo "=== ps -L ==="; ps -L -p "$PID" -o pid,tid,psr,pcpu,stat,comm,wchan:32 2>&1 || true; } > "$OUT_DIR/$1"
}
thread_snapshot threads-before.txt

# ── FlameGraph toolkit — fetch once, reuse forever ───────────────────────
# Two small, standalone Perl scripts (no build step, no heavy deps), from
# the canonical Brendan Gregg FlameGraph repo. This is the actual "render a
# real flame graph" step: perf's own `perf report`/`perf script` only
# produce text tables, never an interactive SVG.
ensure_flamegraph_toolkit() {
    command -v perl >/dev/null 2>&1 || { echo "perl not installed — cannot render an SVG flame graph" >&2; return 1; }
    mkdir -p "$FG_DIR"
    local f
    for f in stackcollapse-perf.pl flamegraph.pl; do
        if [[ ! -s "$FG_DIR/$f" ]]; then
            echo "==> fetching $f (FlameGraph toolkit, one-time)..."
            curl -sfL -o "$FG_DIR/$f" \
                "https://raw.githubusercontent.com/brendangregg/FlameGraph/$FG_REVISION/$f" \
                || { echo "failed to fetch $f from raw.githubusercontent.com" >&2; return 1; }
        fi
    done
    chmod +x "$FG_DIR"/*.pl
}

# Runs `perf record` (args in $*, plus -o "$perf_data") bounded by
# --stop-file if set (background perf, poll once/sec, SIGINT it as soon as
# the file appears — --duration is still the safety-cap ceiling so a
# caller that never creates the file can't hang this forever) or by plain
# `-- sleep $DURATION` otherwise (the original, still-default behavior for
# a caller with no early-stop signal to give — kept as its own simple,
# synchronous path rather than folded into the polling one, since `timeout`
# can wrap that directly and a shell function can't be `timeout`'d without
# `export -f` gymnastics).
run_perf_record() {
    local perf_data="$1"; shift
    if [[ -z "$STOP_FILE" ]]; then
        local timeout_cmd=()
        command -v timeout >/dev/null 2>&1 && timeout_cmd=(timeout "$((DURATION + 15))")
        "${timeout_cmd[@]}" perf record "$@" -o "$perf_data" -- sleep "$DURATION"
        return $?
    fi
    perf record "$@" -o "$perf_data" &
    local perf_pid=$!
    local waited=0
    while [[ $waited -lt $DURATION ]]; do
        if [[ -e "$STOP_FILE" ]]; then
            echo "==> stop-file seen after ${waited}s; stopping perf record early (--duration was ${DURATION}s)"
            break
        fi
        if ! kill -0 "$perf_pid" 2>/dev/null; then
            break # perf itself already exited (a real failure) -- nothing left to wait on
        fi
        sleep 1
        waited=$((waited + 1))
    done
    kill -INT "$perf_pid" 2>/dev/null || true
    wait "$perf_pid"
}

capture_with_perf() {
    command -v perf >/dev/null 2>&1 || return 1
    local perf_data="$OUT_DIR/perf.data"

    echo "==> perf available; recording DWARF call stacks"
    rm -f "$perf_data"
    run_perf_record "$perf_data" -e "${PROFILE_EVENT:-cycles}" -F "${PROFILE_FREQUENCY:-99}" --call-graph "${PROFILE_CALL_GRAPH:-dwarf}" -p "$PID" \
        > "$OUT_DIR/perf-record.txt" 2>&1
    local record_status=$?
    if [[ "$record_status" -ne 0 || ! -s "$perf_data" ]]; then
        echo "DWARF call-graph capture failed; retrying with frame-pointer stacks" >> "$OUT_DIR/perf-record.txt"
        rm -f "$perf_data"
        run_perf_record "$perf_data" -e "${PROFILE_EVENT:-cycles}" -F "${PROFILE_FREQUENCY:-99}" -g -p "$PID" \
            >> "$OUT_DIR/perf-record.txt" 2>&1
        [[ $? -eq 0 ]] || return 1
    fi
    [[ -s "$perf_data" ]] || return 1
    PROFILE_METHOD="perf"
    # A stack-wide caller renders separately after all captures complete.
    [[ "${PROFILE_CAPTURE_ONLY:-0}" != 1 ]] || return 0

    # --no-inline on every perf report/script call below, deliberately.
    # Found live: perf's default inline-frame resolution shells out to
    # `addr2line -i` against the profiled binary's DWARF info, and for a
    # real Rust debug binary (heavy generic-monomorphization inlining) that
    # single addr2line invocation spiked to 828MB RSS / 91% CPU and
    # swap-thrashed a 1.9GB box for 46 minutes before it was killed — 12s
    # of actual CPU time consumed across that whole wall-clock span, i.e.
    # it wasn't slow, it was thrashing. --no-inline trades precise leaf
    # attribution for a heavily-inlined generic function (samples can land
    # on a nearby symbol instead of the true call site) for a capture that
    # actually completes. Re-enable --inline only on a box with real memory
    # headroom relative to the binary's debug-info size.
    perf report --stdio --no-inline -i "$perf_data" --sort comm,dso,symbol --percent-limit 0 \
        > "$OUT_DIR/perf-report.txt" 2>&1 || true
    perf report --stdio --no-children --no-inline -i "$perf_data" --percent-limit 0 \
        > "$OUT_DIR/perf-self-report.txt" 2>&1 || true
    perf script --no-inline -i "$perf_data" > "$OUT_DIR/perf.script" 2> "$OUT_DIR/perf-script.txt" || true
    if command -v rustfilt >/dev/null 2>&1; then
        rustfilt < "$OUT_DIR/perf.script" > "$OUT_DIR/perf-rustfilt.script" 2> "$OUT_DIR/rustfilt.txt" || true
    fi

    if [[ -s "$OUT_DIR/perf.script" ]] && ensure_flamegraph_toolkit; then
        local script_for_fg="$OUT_DIR/perf.script"
        [[ -s "$OUT_DIR/perf-rustfilt.script" ]] && script_for_fg="$OUT_DIR/perf-rustfilt.script"
        echo "==> rendering flamegraph.svg from $(basename "$script_for_fg")"
        perl "$FG_DIR/stackcollapse-perf.pl" "$script_for_fg" > "$OUT_DIR/out.folded" 2>"$OUT_DIR/stackcollapse.err"
        if [[ -s "$OUT_DIR/out.folded" ]]; then
            perl "$FG_DIR/flamegraph.pl" --title "$LABEL CPU flame graph (${DURATION}s sample)" \
                "$OUT_DIR/out.folded" > "$OUT_DIR/flamegraph.svg" 2>"$OUT_DIR/flamegraph.err"
            [[ -s "$OUT_DIR/flamegraph.svg" ]] || echo "flamegraph.pl produced no output — see flamegraph.err" >&2
        else
            echo "stackcollapse-perf.pl produced no folded stacks — see stackcollapse.err" >&2
        fi
    fi
    PROFILE_METHOD="perf"
    return 0
}

summarize_perf() {
    local report
    for report in "$OUT_DIR/perf-self-report.txt" "$OUT_DIR/perf-report.txt"; do
        [[ -s "$report" ]] || continue
        awk '
            /^[[:space:]]*[0-9]+(\.[0-9]+)?%/ {
                line = $0; pct = line
                sub(/^[[:space:]]*/, "", pct); sub(/%.*/, "", pct)
                sub(/^.*\[[^]]+\][[:space:]]*/, "", line)
                if (line != "") printf "%6s%%  %s\n", pct, line
            }
        ' "$report" | sort -nr | head -20 > "$OUT_DIR/top-functions.txt"
        [[ -s "$OUT_DIR/top-functions.txt" ]] && return 0
    done
    return 1
}

capture_with_strace() {
    command -v strace >/dev/null 2>&1 || return 1
    echo "==> perf unavailable or blocked; collecting syscall fallback with strace"
    local timeout_cmd=()
    command -v timeout >/dev/null 2>&1 && timeout_cmd=(timeout "$((DURATION + 15))")
    "${timeout_cmd[@]}" strace -f -c -p "$PID" -o "$OUT_DIR/strace-summary.txt" > "$OUT_DIR/strace.txt" 2>&1 || return 1
    [[ -s "$OUT_DIR/strace-summary.txt" ]] || return 1
    PROFILE_METHOD="strace-fallback"
    return 0
}
summarize_strace() {
    [[ -s "$OUT_DIR/strace-summary.txt" ]] || return 1
    awk 'NR == 1 || /^[[:space:]]*[0-9]/ { print }' "$OUT_DIR/strace-summary.txt" | head -15 > "$OUT_DIR/top-syscalls.txt"
    [[ -s "$OUT_DIR/top-syscalls.txt" ]]
}

PROFILE_METHOD="none"
if ! capture_with_perf; then
    if [[ "${PROFILE_REQUIRE_PERF:-0}" == 1 ]]; then
        echo "perf capture failed; refusing to label fallback data a CPU profile" >&2
        exit 1
    fi
    if ! capture_with_strace; then
        echo "WARNING: neither perf nor strace is available/usable; sleeping for the sample window." >&2
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

if [[ -n "$JOURNAL_UNIT" ]] && command -v journalctl >/dev/null 2>&1; then
    journalctl -u "$JOURNAL_UNIT" --since "$START_ISO" --until "$END_ISO" --no-pager -o short-iso-precise \
        > "$OUT_DIR/$LABEL-journal.txt" 2>&1 || true
fi

{
    echo "$LABEL CPU profile"; echo "=================="
    echo "pid: $PID"; echo "started: $START_ISO"; echo "ended: $END_ISO"
    echo "duration_seconds: $DURATION"; echo "profile_method: $PROFILE_METHOD"
    echo "executable: $EXE"; echo "cmdline: $CMDLINE"; echo
    if [[ -n "$JOURNAL_UNIT" ]]; then
        WARN_COUNT=0; ERR_COUNT=0
        if [[ -f "$OUT_DIR/$LABEL-journal.txt" ]]; then
            WARN_COUNT="$(grep -ciE 'WARN' "$OUT_DIR/$LABEL-journal.txt" || true)"
            ERR_COUNT="$(grep -ciE 'ERROR|error=' "$OUT_DIR/$LABEL-journal.txt" || true)"
        fi
        echo "journal ($JOURNAL_UNIT) warnings: $WARN_COUNT   errors: $ERR_COUNT"
        echo
    fi
    echo "files:"; find "$OUT_DIR" -maxdepth 1 -type f -printf '  %f\n' | sort
} > "$OUT_DIR/SUMMARY.txt"

# perf needs real privilege (this script is typically run under sudo), so
# every file it and this script wrote under $OUT_DIR is root-owned. A
# caller that isn't root itself (a CI step that uploads $OUT_DIR as an
# unprivileged user, e.g.) can't even read them back — found live:
# actions/upload-artifact failed with EACCES on perf.data. Hand the
# directory back to whoever actually invoked sudo, same as any
# well-behaved sudo-wrapped tool should.
if [[ -n "${SUDO_UID:-}" && -n "${SUDO_GID:-}" ]]; then
    chown -R "$SUDO_UID:$SUDO_GID" "$OUT_DIR" 2>/dev/null || true
fi

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
if [[ -s "$OUT_DIR/flamegraph.svg" ]]; then
    echo
    echo "==> real flame graph: $OUT_DIR/flamegraph.svg"
fi
echo
echo "===== busiest threads after capture ====="
ps -L -p "$PID" --sort=-pcpu -o tid,psr,pcpu,stat,comm,wchan:32 2>&1 | head -15 || true
