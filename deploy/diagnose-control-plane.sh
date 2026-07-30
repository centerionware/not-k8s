#!/usr/bin/env bash
# diagnose-control-plane.sh — find out what inside k3s-server is actually
# burning CPU/RAM at idle, on a real running deployment.
#
# nodelet and bootstrap-source.sh only ever touched the node-agent side (the
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
    echo "=== argv (/proc/$K3S_PID/cmdline) ==="
    tr '\0' ' ' < "/proc/$K3S_PID/cmdline" 2>/dev/null; echo
    echo "(if this is just \"k3s server\" with no flags, that does NOT mean no flags are \
in effect — k3s also reads /etc/rancher/k3s/config.yaml. Checked below.)"
    echo
    echo "=== systemd unit (resolved ExecStart + ExecStart as written) ==="
    if command -v systemctl &>/dev/null; then
        systemctl show k3s.service -p ExecStart 2>&1
        echo
        systemctl cat k3s.service 2>&1
    else
        echo "(no systemctl — not running under systemd?)"
    fi
    echo
    echo "=== /etc/rancher/k3s/config.yaml ==="
    cat /etc/rancher/k3s/config.yaml 2>&1 || echo "(not present)"
    echo
    echo "=== goroutine/thread count ==="
    echo "OS threads: $(ls /proc/$K3S_PID/task 2>/dev/null | wc -l)"
} > "$OUT_DIR/00-process-info.txt" 2>&1

# ── Sanity check: are the --disable-* flags actually doing anything? ────
# Directly observable regardless of how flags were passed (argv vs. config
# file, which the section above may or may not resolve depending on k3s
# version): if traefik/servicelb/local-storage/metrics-server are disabled,
# there should be no pods for them. If they're still running, that alone
# could explain most of the reported CPU/RAM — this checks it instead of
# assuming setup-control-plane.sh's flags took effect.
if command -v kubectl &>/dev/null && [[ -f "$KUBECONFIG" ]]; then
    {
        echo "=== nodes ==="
        KUBECONFIG="$KUBECONFIG" kubectl get nodes -o wide 2>&1
        echo
        echo "=== all pods (should be ~empty on a freshly-stripped control plane; \
traefik/svclb/local-path-provisioner/metrics-server pods here mean the --disable \
flags are NOT in effect) ==="
        KUBECONFIG="$KUBECONFIG" kubectl get pods -A -o wide 2>&1
        echo
        echo "=== leases (each one renews periodically — component leases \
(kube-controller-manager, kube-scheduler) are expected; extras are worth knowing about) ==="
        KUBECONFIG="$KUBECONFIG" kubectl get leases -A 2>&1
    } > "$OUT_DIR/00b-disable-flags-sanity-check.txt" 2>&1
fi

# ── Is nodelet actually running? ─────────────────────────────────────────
# A NotReady node / pods stuck Pending forever points at nodelet, not
# k3s-server's CPU/RAM — a different problem than what this script exists
# to diagnose, but worth surfacing plainly instead of leaving it implicit
# in a NotReady status buried in the section above.
{
    echo "=== nodelet process ==="
    pgrep -af nodelet 2>&1 || echo "no nodelet process found"
} > "$OUT_DIR/00c-nodelet-status.txt" 2>&1

# ── journalctl: the actual error behind any retry-looping controller ────
# workqueue_retries_total climbing in lockstep with workqueue_adds_total
# (checked below) says *that* a controller is stuck failing and retrying,
# not *why*. The real error is only in the log. Two windows: the last 500
# lines (usually startup on a quiet box — still useful) and a live window
# covering the actual sample period, since "retries" climbing with zero
# recent ERROR lines is itself the signal that it's a resync, not a failure.
if command -v journalctl &>/dev/null; then
    journalctl -u k3s --no-pager -n 500 2>&1 \
        | grep -iE 'openapi|aggregat|error|failed' \
        > "$OUT_DIR/00d-journal-errors.txt" || true
    journalctl -u k3s --no-pager --since "${DURATION} seconds ago" 2>&1 \
        > "$OUT_DIR/00e-journal-live-window.txt" || true
fi

# ── Is a CRD actually churning? (feeds the openapi v3 aggregation queue) ─
# apiserver_request_body_size_bytes_sum showing customresourcedefinitions
# update traffic means *something* periodically writes to a CRD — every
# such write is exactly what re-triggers the OpenAPI v3 aggregation
# controller's queue for that CRD's schema. Sampling resourceVersion twice
# confirms whether that's actually happening right now.
if command -v kubectl &>/dev/null && [[ -f "$KUBECONFIG" ]]; then
    {
        echo "=== CustomResourceDefinitions (resourceVersion sampled twice, 5s apart) ==="
        KUBECONFIG="$KUBECONFIG" kubectl get crd -o custom-columns='NAME:.metadata.name,RESOURCEVERSION:.metadata.resourceVersion,GENERATION:.metadata.generation' 2>&1
        sleep 5
        echo "--- 5s later ---"
        KUBECONFIG="$KUBECONFIG" kubectl get crd -o custom-columns='NAME:.metadata.name,RESOURCEVERSION:.metadata.resourceVersion,GENERATION:.metadata.generation' 2>&1
        echo
        echo "=== APIServices (aggregated API groups — separate contributor to the same controller) ==="
        KUBECONFIG="$KUBECONFIG" kubectl get apiservices 2>&1
    } > "$OUT_DIR/00f-crd-churn.txt" 2>&1
fi

# ── Per-thread CPU snapshot ─────────────────────────────────────────────

log "Sampling per-thread CPU (3 snapshots, 2s apart)..."
{
    for i in 1 2 3; do
        echo "=== snapshot $i ==="
        top -H -b -n1 -p "$K3S_PID" 2>&1
        sleep 2
    done
} > "$OUT_DIR/01-top-threads.txt" 2>&1

# Real pprof profiles are gzip data (magic bytes 1f 8b) — the one check
# that reliably tells a genuine profile apart from an error page/JSON body
# without needing to know the specific failure mode in advance.
is_gzip() {
    local size=0
    [[ -f "$1" ]] && size=$(stat -c%s "$1" 2>/dev/null || echo 0)
    [[ "$size" -ge 2 ]] || return 1
    [[ "$(head -c2 "$1" | od -An -tx1 | tr -d ' ')" == "1f8b" ]]
}

# A captured "profile" that's actually an auth error, a 404, or profiling
# being disabled still writes *a* file — go tool pprof just fails on it
# later with an opaque "unrecognized profile format", which doesn't say
# why. If it's not real gzip'd profile data, dump its size, stderr, and
# response body (short for an error page) into <name>-diagnosis.txt instead
# of ever handing it to go tool pprof, so a failure is self-explanatory in
# SUMMARY.txt without a second round trip just to read an error message.
diagnose_pprof_capture() {
    local file="$1" name="$2" errfile="$3"
    is_gzip "$file" && return 0
    local size=0
    [[ -f "$file" ]] && size=$(stat -c%s "$file" 2>/dev/null || echo 0)

    {
        echo "$name: did not produce a valid pprof profile (size: ${size} bytes)."
        if [[ -f "$errfile" && -s "$errfile" ]]; then
            echo "--- stderr ---"
            cat "$errfile"
        fi
        if [[ "$size" -gt 0 && "$size" -lt 5000 ]]; then
            echo "--- response body (first 2000 bytes — likely an error page, not a profile) ---"
            head -c 2000 "$file"
            echo
        fi
    } > "$OUT_DIR/$name-diagnosis.txt"
    return 1
}

# ── Go pprof: apiserver (via kubectl, so auth is handled automatically) ─

if command -v kubectl &>/dev/null && [[ -f "$KUBECONFIG" ]]; then
    log "Capturing apiserver CPU profile (${DURATION}s, via kubectl)..."
    KUBECONFIG="$KUBECONFIG" kubectl get --raw "/debug/pprof/profile?seconds=$DURATION" \
        > "$OUT_DIR/apiserver-cpu.pprof" 2>"$OUT_DIR/apiserver-cpu.err" &
    APISERVER_CPU_PID=$!

    log "Capturing apiserver goroutine dump + heap profile..."
    KUBECONFIG="$KUBECONFIG" kubectl get --raw /debug/pprof/goroutine?debug=2 \
        > "$OUT_DIR/apiserver-goroutines.txt" 2>"$OUT_DIR/apiserver-goroutines.err"
    KUBECONFIG="$KUBECONFIG" kubectl get --raw /debug/pprof/heap \
        > "$OUT_DIR/apiserver-heap.pprof" 2>"$OUT_DIR/apiserver-heap.err"

    log "Capturing apiserver /metrics..."
    KUBECONFIG="$KUBECONFIG" kubectl get --raw /metrics > "$OUT_DIR/apiserver-metrics.raw" 2>"$OUT_DIR/apiserver-metrics.err"

    wait "$APISERVER_CPU_PID" 2>/dev/null
    diagnose_pprof_capture "$OUT_DIR/apiserver-cpu.pprof" "apiserver-cpu" "$OUT_DIR/apiserver-cpu.err"
    diagnose_pprof_capture "$OUT_DIR/apiserver-heap.pprof" "apiserver-heap" "$OUT_DIR/apiserver-heap.err"
else
    warn "kubectl or $KUBECONFIG not found — skipping apiserver pprof/metrics."
fi

# ── Go pprof: controller-manager + scheduler (secure ports, need a cert) ─

CLIENT_CERT="$TLS_DIR/client-admin.crt"
CLIENT_KEY="$TLS_DIR/client-admin.key"
CA_CERT="$TLS_DIR/server-ca.crt"

if [[ -f "$CLIENT_CERT" && -f "$CLIENT_KEY" && -f "$CA_CERT" ]]; then
    CURL_AUTH=(-sk --cacert "$CA_CERT" --cert "$CLIENT_CERT" --key "$CLIENT_KEY" -w '\nHTTP_STATUS:%{http_code}\n')

    log "Capturing controller-manager CPU profile (${DURATION}s, port 10257)..."
    curl "${CURL_AUTH[@]}" "https://localhost:10257/debug/pprof/profile?seconds=$DURATION" \
        -o "$OUT_DIR/cm-cpu.pprof" >"$OUT_DIR/cm-cpu.err" 2>&1 &
    CM_CPU_PID=$!

    curl "${CURL_AUTH[@]}" "https://localhost:10257/debug/pprof/goroutine?debug=2" \
        -o "$OUT_DIR/cm-goroutines.txt" >"$OUT_DIR/cm-goroutines.err" 2>&1
    curl "${CURL_AUTH[@]}" "https://localhost:10257/metrics" \
        -o "$OUT_DIR/cm-metrics.raw" >"$OUT_DIR/cm-metrics.err" 2>&1

    log "Capturing scheduler CPU profile (${DURATION}s, port 10259)..."
    curl "${CURL_AUTH[@]}" "https://localhost:10259/debug/pprof/profile?seconds=$DURATION" \
        -o "$OUT_DIR/scheduler-cpu.pprof" >"$OUT_DIR/scheduler-cpu.err" 2>&1 &
    SCHED_CPU_PID=$!

    wait "$CM_CPU_PID" "$SCHED_CPU_PID" 2>/dev/null
    diagnose_pprof_capture "$OUT_DIR/cm-cpu.pprof" "cm-cpu" "$OUT_DIR/cm-cpu.err"
    diagnose_pprof_capture "$OUT_DIR/scheduler-cpu.pprof" "scheduler-cpu" "$OUT_DIR/scheduler-cpu.err"
else
    warn "k3s's admin client cert not found at $TLS_DIR — skipping controller-manager/scheduler pprof. \
(Layout may differ between k3s versions; the apiserver profile above is the most important one anyway.)"
fi

# ── Convert every captured .pprof into a human-readable top-N text file ─

if command -v go &>/dev/null; then
    for f in "$OUT_DIR"/*.pprof; do
        [[ -s "$f" ]] || continue
        # Already known bad (diagnose_pprof_capture wrote a diagnosis for
        # it) — don't hand it to go tool pprof just to get its opaque
        # "unrecognized profile format" instead of the real reason.
        is_gzip "$f" || continue
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
# Metric names have moved before (Kubernetes metrics stability churn), so
# if the expected names come up empty, fall back to whatever workqueue/
# request metrics actually exist rather than silently showing nothing.

extract_top_metrics() {
    local raw="$1" out="$2"
    [[ -s "$raw" ]] || return 0
    grep -E '^(apiserver_request_total|workqueue_adds_total|workqueue_depth) ' "$raw" 2>/dev/null \
        | sort -k2 -n -r 2>/dev/null | head -30 > "$out"
    if [[ ! -s "$out" ]]; then
        { echo "(expected metric names not found; showing any workqueue/request metrics present instead)"
          grep -E '^(workqueue_|apiserver_request)' "$raw" 2>/dev/null | sort -k2 -n -r 2>/dev/null | head -30
        } > "$out"
    fi
}
extract_top_metrics "$OUT_DIR/apiserver-metrics.raw" "$OUT_DIR/apiserver-metrics-top.txt"
extract_top_metrics "$OUT_DIR/cm-metrics.raw" "$OUT_DIR/cm-metrics-top.txt"

# workqueue_retries_total climbing at close to the same rate as
# workqueue_adds_total means a controller is failing and re-queuing almost
# every single attempt — a stuck fail-loop, not idle housekeeping, and the
# single most actionable signal this script can surface. Its own section
# so it isn't buried under the (much more numerous) apiserver_request_*
# watch-duration entries in the generic metrics dump above.
extract_retry_loops() {
    local raw="$1" out="$2"
    [[ -s "$raw" ]] || return 0
    grep -E '^workqueue_retries_total\{' "$raw" 2>/dev/null | sort -k2 -n -r 2>/dev/null | head -20 > "$out"
}
extract_retry_loops "$OUT_DIR/apiserver-metrics.raw" "$OUT_DIR/apiserver-retries.txt"
extract_retry_loops "$OUT_DIR/cm-metrics.raw" "$OUT_DIR/cm-retries.txt"

# ── Pull it together into one pasteable summary ─────────────────────────

SUMMARY="$OUT_DIR/SUMMARY.txt"
{
    echo "not-k8s control-plane diagnostic — $(date)"
    echo "======================================================"
    echo
    echo "--- process ---"
    cat "$OUT_DIR/00-process-info.txt" 2>/dev/null
    echo
    echo "--- are the --disable flags actually in effect? ---"
    if [[ -f "$OUT_DIR/00b-disable-flags-sanity-check.txt" ]]; then
        cat "$OUT_DIR/00b-disable-flags-sanity-check.txt"
    else
        echo "(not captured — no kubectl/KUBECONFIG)"
    fi
    echo
    echo "--- is nodelet running? (NotReady node / stuck Pending pods point here, not at k3s-server) ---"
    cat "$OUT_DIR/00c-nodelet-status.txt" 2>/dev/null
    echo
    echo "--- apiserver CPU profile: top 25 functions ---"
    if [[ -f "$OUT_DIR/apiserver-cpu-top.txt" ]]; then
        cat "$OUT_DIR/apiserver-cpu-top.txt"
    elif [[ -f "$OUT_DIR/apiserver-cpu-diagnosis.txt" ]]; then
        cat "$OUT_DIR/apiserver-cpu-diagnosis.txt"
    else
        echo "(not captured)"
    fi
    echo
    echo "--- controller-manager CPU profile: top 25 functions ---"
    if [[ -f "$OUT_DIR/cm-cpu-top.txt" ]]; then
        cat "$OUT_DIR/cm-cpu-top.txt"
    elif [[ -f "$OUT_DIR/cm-cpu-diagnosis.txt" ]]; then
        cat "$OUT_DIR/cm-cpu-diagnosis.txt"
    else
        echo "(not captured)"
    fi
    echo
    echo "--- scheduler CPU profile: top 25 functions ---"
    if [[ -f "$OUT_DIR/scheduler-cpu-top.txt" ]]; then
        cat "$OUT_DIR/scheduler-cpu-top.txt"
    elif [[ -f "$OUT_DIR/scheduler-cpu-diagnosis.txt" ]]; then
        cat "$OUT_DIR/scheduler-cpu-diagnosis.txt"
    else
        echo "(not captured)"
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
    echo "--- retry loops: workqueue_retries_total close to workqueue_adds_total means a ---"
    echo "--- controller is stuck failing and re-queuing continuously, not idling ---"
    echo "(apiserver)"
    cat "$OUT_DIR/apiserver-retries.txt" 2>/dev/null
    echo "(controller-manager)"
    cat "$OUT_DIR/cm-retries.txt" 2>/dev/null
    echo
    echo "--- journal errors near 'openapi'/'aggregat'/'error'/'failed' (last 500 lines) ---"
    if [[ -s "$OUT_DIR/00d-journal-errors.txt" ]]; then
        tail -60 "$OUT_DIR/00d-journal-errors.txt"
    else
        echo "(none found, or journalctl unavailable)"
    fi
    echo
    echo "--- live journal window covering this sample's ${DURATION}s (retries climbing with NO ---"
    echo "--- corresponding log lines here means it's a resync, not a failure) ---"
    if [[ -s "$OUT_DIR/00e-journal-live-window.txt" ]]; then
        cat "$OUT_DIR/00e-journal-live-window.txt"
    else
        echo "(empty — no log activity during the sample window at all)"
    fi
    echo
    echo "--- is a CRD actually churning? (each write re-triggers the openapi v3 aggregation queue) ---"
    if [[ -f "$OUT_DIR/00f-crd-churn.txt" ]]; then
        cat "$OUT_DIR/00f-crd-churn.txt"
    else
        echo "(not captured — no kubectl/KUBECONFIG)"
    fi
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
