#!/usr/bin/env bash
# diagnose-control-plane.sh — find out what inside k3s-server is actually
# burning CPU/RAM at idle, on a real running deployment.
#
# nodelet and bootstrap-test.sh only ever touched the node-agent side (the
# kubelet replacement). k3s-server itself — apiserver + scheduler +
# controller-manager + kine (its embedded sqlite datastore), all bundled
# into one process — has never been profiled or trimmed. This script
# collects the evidence needed to do that: real Go pprof CPU/heap profiles
# from each embedded component, a goroutine dump, a syscall summary (to
# catch kine's SQLite polling — a known, well-documented source of both
# idle CPU *and* flash writes on single-node k3s), and workqueue/request
# metrics.
#
# Usage:
#   sudo ./deploy/diagnose-control-plane.sh [seconds]   # default 30s sample
#
# Output: a timestamped directory under /tmp (path printed at the end),
# plus a SUMMARY.txt whose contents are also printed directly to stdout —
# paste that back. The full directory is also tar'd up in case a deeper
# look is needed.
set -uo pipefail   # deliberately not -e: every section is independent and
                    # best-effort; one failing check must not abort the rest.

DURATION="${1:-30}"
KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"
TLS_DIR=/var/lib/rancher/k3s/server/tls
DB_DIR=/var/lib/rancher/k3s/server/db
OUT_DIR="/tmp/not-k8s-diag-$(date +%Y%m%d-%H%M%S)"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==> WARNING:\033[0m %s\n' "$*" >&2; }

[[ "$EUID" -eq 0 ]] || { echo "Run as root (needed for strace -p and reading k3s's TLS certs): sudo $0" >&2; exit 1; }

K3S_PID="$(pgrep -f 'k3s server' | head -1)"
[[ -n "$K3S_PID" ]] || { echo "No 'k3s server' process found. Is it running?" >&2; exit 1; }

mkdir -p "$OUT_DIR"
log "k3s-server pid=$K3S_PID — collecting a ${DURATION}s sample into $OUT_DIR"

# Best-effort tool install (Debian/Ubuntu only; every section below
# degrades gracefully — checked via command -v — if a tool isn't present
# and apt isn't either).
if command -v apt-get &>/dev/null; then
    NEEDED=""
    command -v strace &>/dev/null || NEEDED="$NEEDED strace"
    command -v go &>/dev/null || NEEDED="$NEEDED golang-go"
    command -v sqlite3 &>/dev/null || NEEDED="$NEEDED sqlite3"
    if [[ -n "$NEEDED" ]]; then
        log "Installing missing tools:$NEEDED"
        apt-get install -y -qq $NEEDED >/dev/null 2>&1 || warn "Some tools failed to install — those sections will be skipped."
    fi
fi

# ── Process-level context ───────────────────────────────────────────────

{
    echo "=== k3s --version ==="
    k3s --version 2>&1
    echo
    echo "=== process info ==="
    ps -o pid,ppid,nlwp,%cpu,%mem,rss,etimes,cmd -p "$K3S_PID"
    echo
    echo "=== how k3s-server was started (disable flags in effect) ==="
    tr '\0' ' ' < "/proc/$K3S_PID/cmdline" 2>/dev/null; echo
    echo
    echo "=== goroutine/thread count ==="
    echo "OS threads: $(ls /proc/$K3S_PID/task 2>/dev/null | wc -l)"
} > "$OUT_DIR/00-process-info.txt" 2>&1

# ── Per-thread CPU snapshot ─────────────────────────────────────────────

log "Sampling per-thread CPU (3 snapshots, 2s apart)..."
{
    for i in 1 2 3; do
        echo "=== snapshot $i ==="
        top -H -b -n1 -p "$K3S_PID" 2>&1
        sleep 2
    done
} > "$OUT_DIR/01-top-threads.txt" 2>&1

# ── Go pprof: apiserver (via kubectl, so auth is handled automatically) ─

if command -v kubectl &>/dev/null && [[ -f "$KUBECONFIG" ]]; then
    log "Capturing apiserver CPU profile (${DURATION}s, via kubectl)..."
    KUBECONFIG="$KUBECONFIG" kubectl get --raw "/debug/pprof/profile?seconds=$DURATION" \
        > "$OUT_DIR/apiserver-cpu.pprof" 2>"$OUT_DIR/apiserver-cpu.err" &
    APISERVER_CPU_PID=$!

    log "Capturing apiserver goroutine dump + heap profile..."
    KUBECONFIG="$KUBECONFIG" kubectl get --raw /debug/pprof/goroutine?debug=2 \
        > "$OUT_DIR/apiserver-goroutines.txt" 2>>"$OUT_DIR/apiserver-cpu.err"
    KUBECONFIG="$KUBECONFIG" kubectl get --raw /debug/pprof/heap \
        > "$OUT_DIR/apiserver-heap.pprof" 2>>"$OUT_DIR/apiserver-cpu.err"

    log "Capturing apiserver /metrics..."
    KUBECONFIG="$KUBECONFIG" kubectl get --raw /metrics > "$OUT_DIR/apiserver-metrics.raw" 2>>"$OUT_DIR/apiserver-cpu.err"

    wait "$APISERVER_CPU_PID" 2>/dev/null
else
    warn "kubectl or $KUBECONFIG not found — skipping apiserver pprof/metrics."
fi

# ── Go pprof: controller-manager + scheduler (secure ports, need a cert) ─

CLIENT_CERT="$TLS_DIR/client-admin.crt"
CLIENT_KEY="$TLS_DIR/client-admin.key"
CA_CERT="$TLS_DIR/server-ca.crt"

if [[ -f "$CLIENT_CERT" && -f "$CLIENT_KEY" && -f "$CA_CERT" ]]; then
    log "Capturing controller-manager CPU profile (${DURATION}s, port 10257)..."
    curl -sk --cacert "$CA_CERT" --cert "$CLIENT_CERT" --key "$CLIENT_KEY" \
        "https://localhost:10257/debug/pprof/profile?seconds=$DURATION" \
        > "$OUT_DIR/cm-cpu.pprof" 2>"$OUT_DIR/cm-cpu.err" &
    CM_CPU_PID=$!

    curl -sk --cacert "$CA_CERT" --cert "$CLIENT_CERT" --key "$CLIENT_KEY" \
        "https://localhost:10257/debug/pprof/goroutine?debug=2" \
        > "$OUT_DIR/cm-goroutines.txt" 2>>"$OUT_DIR/cm-cpu.err"
    curl -sk --cacert "$CA_CERT" --cert "$CLIENT_CERT" --key "$CLIENT_KEY" \
        "https://localhost:10257/metrics" > "$OUT_DIR/cm-metrics.raw" 2>>"$OUT_DIR/cm-cpu.err"

    log "Capturing scheduler CPU profile (${DURATION}s, port 10259)..."
    curl -sk --cacert "$CA_CERT" --cert "$CLIENT_CERT" --key "$CLIENT_KEY" \
        "https://localhost:10259/debug/pprof/profile?seconds=$DURATION" \
        > "$OUT_DIR/scheduler-cpu.pprof" 2>"$OUT_DIR/scheduler-cpu.err" &
    SCHED_CPU_PID=$!

    wait "$CM_CPU_PID" "$SCHED_CPU_PID" 2>/dev/null
else
    warn "k3s's admin client cert not found at $TLS_DIR — skipping controller-manager/scheduler pprof. \
(Layout may differ between k3s versions; the apiserver profile above is the most important one anyway.)"
fi

# ── Convert every captured .pprof into a human-readable top-N text file ─

if command -v go &>/dev/null; then
    for f in "$OUT_DIR"/*.pprof; do
        [[ -s "$f" ]] || continue
        name="$(basename "$f" .pprof)"
        log "Rendering $name..."
        go tool pprof -top -nodecount=25 "$f" > "$OUT_DIR/$name-top.txt" 2>&1
    done
else
    warn "go not available — .pprof files saved raw; run 'go tool pprof -top file.pprof' wherever Go is installed to read them."
fi

# ── strace summary: catches kine's SQLite polling directly ─────────────

if command -v strace &>/dev/null; then
    log "Running strace -c -f for ${DURATION}s (syscall summary)..."
    timeout "$((DURATION + 2))" strace -f -c -p "$K3S_PID" -o "$OUT_DIR/02-strace-summary.txt" 2>/dev/null
    sleep "$DURATION"
else
    warn "strace not available — skipping syscall summary."
fi

# ── kine/SQLite write activity: does state.db-wal churn at idle? ───────

if [[ -d "$DB_DIR" ]]; then
    log "Sampling state.db(-wal) size/mtime twice, ${DURATION}s apart..."
    {
        echo "=== before ==="
        ls -la --time-style=full-iso "$DB_DIR"/state.db* 2>&1
        [[ -f "$DB_DIR/state.db" ]] && command -v sqlite3 &>/dev/null && {
            echo "row counts (kine table):"
            sqlite3 "$DB_DIR/state.db" "SELECT COUNT(*) FROM kine;" 2>&1
        }
    } > "$OUT_DIR/03-sqlite-activity.txt" 2>&1
    sleep "$DURATION"
    {
        echo
        echo "=== after (${DURATION}s later) ==="
        ls -la --time-style=full-iso "$DB_DIR"/state.db* 2>&1
        [[ -f "$DB_DIR/state.db" ]] && command -v sqlite3 &>/dev/null && {
            echo "row counts (kine table):"
            sqlite3 "$DB_DIR/state.db" "SELECT COUNT(*) FROM kine;" 2>&1
        }
    } >> "$OUT_DIR/03-sqlite-activity.txt" 2>&1
else
    warn "$DB_DIR not found — k3s may be using a different datastore backend."
fi

# ── Metrics: top request/workqueue counters ─────────────────────────────

if [[ -s "$OUT_DIR/apiserver-metrics.raw" ]]; then
    grep -E '^(apiserver_request_total|workqueue_adds_total|workqueue_depth) ' "$OUT_DIR/apiserver-metrics.raw" 2>/dev/null \
        | sort -k2 -n -r | head -30 > "$OUT_DIR/apiserver-metrics-top.txt"
fi
if [[ -s "$OUT_DIR/cm-metrics.raw" ]]; then
    grep -E '^workqueue_(adds_total|depth) ' "$OUT_DIR/cm-metrics.raw" 2>/dev/null \
        | sort -k2 -n -r | head -30 > "$OUT_DIR/cm-metrics-top.txt"
fi

# ── Pull it together into one pasteable summary ─────────────────────────

SUMMARY="$OUT_DIR/SUMMARY.txt"
{
    echo "not-k8s control-plane diagnostic — $(date)"
    echo "======================================================"
    echo
    echo "--- process ---"
    cat "$OUT_DIR/00-process-info.txt" 2>/dev/null
    echo
    echo "--- apiserver CPU profile: top 25 functions ---"
    if [[ -f "$OUT_DIR/apiserver-cpu-top.txt" ]]; then
        cat "$OUT_DIR/apiserver-cpu-top.txt"
    else
        echo "(not captured — see apiserver-cpu.err)"
    fi
    echo
    echo "--- controller-manager CPU profile: top 25 functions ---"
    if [[ -f "$OUT_DIR/cm-cpu-top.txt" ]]; then
        cat "$OUT_DIR/cm-cpu-top.txt"
    else
        echo "(not captured — see cm-cpu.err)"
    fi
    echo
    echo "--- scheduler CPU profile: top 25 functions ---"
    if [[ -f "$OUT_DIR/scheduler-cpu-top.txt" ]]; then
        cat "$OUT_DIR/scheduler-cpu-top.txt"
    else
        echo "(not captured — see scheduler-cpu.err)"
    fi
    echo
    echo "--- strace syscall summary (top of file) ---"
    if [[ -f "$OUT_DIR/02-strace-summary.txt" ]]; then
        head -25 "$OUT_DIR/02-strace-summary.txt"
    else
        echo "(not captured — strace unavailable)"
    fi
    echo
    echo "--- sqlite/kine activity (before vs. after) ---"
    cat "$OUT_DIR/03-sqlite-activity.txt" 2>/dev/null
    echo
    echo "--- apiserver: busiest request/workqueue metrics ---"
    cat "$OUT_DIR/apiserver-metrics-top.txt" 2>/dev/null
    echo
    echo "--- controller-manager: busiest workqueues ---"
    cat "$OUT_DIR/cm-metrics-top.txt" 2>/dev/null
} > "$SUMMARY" 2>&1

tar -czf "$OUT_DIR.tar.gz" -C "$(dirname "$OUT_DIR")" "$(basename "$OUT_DIR")" 2>/dev/null

echo
echo "════════════════════════════════════════════════════════════"
echo " Full output: $OUT_DIR"
echo " Tarball:     $OUT_DIR.tar.gz"
echo "════════════════════════════════════════════════════════════"
echo
cat "$SUMMARY"
