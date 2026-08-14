#!/usr/bin/env bash
# lib/upstream-kube-scheduler.sh — install and run a REAL, standalone upstream
# kube-scheduler against the same stripped k3s control plane nodescheduler
# uses, so a profiling run can measure kube-scheduler's own idle RSS/CPU as a
# genuinely separate OS process.
#
# The third sibling of lib/upstream-kubelet.sh and lib/upstream-kube-proxy.sh,
# and for exactly the same reason: k3s runs the scheduler as an embedded
# goroutine inside the same OS process as the apiserver, controller-manager
# and kine, so there is no way to isolate "the scheduler's own number" from a
# stock k3s process at the OS level. This gives it the footing nodescheduler
# already has — its own process, the same control plane, the same node.
#
# # Why this comparison needs more care than the other two
#
# kubelet and kube-proxy are node components: they do steady-state work
# forever (PLEG, cAdvisor housekeeping, iptables resync), so "idle" is a
# meaningful and stable measurement almost immediately.
#
# A scheduler at idle is doing *nothing* — no pending pods means no scheduling
# cycles. Its resident cost is almost entirely informer caches plus the Go
# runtime's own floor, and its CPU cost is watch-event decoding. That makes
# two things true that the report must not gloss over:
#
#   * The RSS comparison is the honest headline. It is dominated by how much
#     of each Pod and Node object each implementation retains — which is the
#     projection argument in docs/SCHEDULER.md, and it is a real, structural
#     difference rather than a tuning artifact.
#   * The idle CPU comparison is only meaningful on a cluster that is
#     *changing*. On a completely static cluster both implementations
#     approach zero and the comparison says nothing. What it does measure,
#     on a cluster with live nodes, is node-heartbeat handling — which is
#     precisely the ActionType bit-decomposition claim.
#
# So a run against an empty, static cluster is expected to show both close to
# zero CPU and to differ mainly in RSS. That is not a failed measurement; it
# is the correct one for that workload.
#
# This is a measurement rig, not a production posture: it reuses k3s's own
# admin kubeconfig rather than issuing a scheduler-specific credential, which
# no real deployment should do.
set -uo pipefail

SCHED_BIN=/usr/local/bin/kube-scheduler
SCHED_WORK_DIR=/var/lib/upstream-kube-scheduler
KUBECONFIG_PATH=/etc/rancher/k3s/k3s.yaml
SERVICE_NAME=upstream-kube-scheduler.service

log() { echo "==> $*"; }

detect_arch() {
    case "$(uname -m)" in
        x86_64) echo amd64 ;;
        aarch64|arm64) echo arm64 ;;
        armv7l) echo arm ;;
        *) return 1 ;;
    esac
}

# Same rule as its two siblings: fetch the kube-scheduler matching the real
# upstream Kubernetes version k3s embeds ("k3s version v1.31.4+k3s1" ->
# v1.31.4), so this is measured against the API generation this control plane
# actually speaks rather than an arbitrary release.
detect_k8s_version() {
    local v
    v="$(k3s --version 2>/dev/null | head -1 | awk '{print $3}')"
    [[ -n "$v" ]] || return 1
    echo "${v%%+*}"
}

install_upstream_kube_scheduler() {
    if [[ -x "$SCHED_BIN" ]]; then
        log "kube-scheduler already installed: $("$SCHED_BIN" --version)"
        return 0
    fi

    local arch k8s_version
    arch="$(detect_arch)" || { echo "unsupported architecture for upstream kube-scheduler: $(uname -m)" >&2; return 1; }
    k8s_version="$(detect_k8s_version)" || { echo "couldn't determine k3s's embedded Kubernetes version from 'k3s --version'" >&2; return 1; }

    log "fetching real upstream kube-scheduler $k8s_version ($arch) — matching this cluster's k3s-embedded Kubernetes version..."
    curl -sfL -o "$SCHED_BIN" "https://dl.k8s.io/release/${k8s_version}/bin/linux/${arch}/kube-scheduler" \
        || { echo "failed to download kube-scheduler $k8s_version for $arch from dl.k8s.io" >&2; return 1; }
    chmod +x "$SCHED_BIN"
    log "installed: $("$SCHED_BIN" --version)"
}

write_scheduler_config() {
    mkdir -p "$SCHED_WORK_DIR"
    # A KubeSchedulerConfiguration file rather than CLI flags, matching how
    # the sibling rigs do it and how a real deployment configures this today.
    #
    # leaderElect is deliberately FALSE. This rig exists to measure a
    # scheduler doing its normal work, and a leader-electing instance that
    # loses the election to k3s's own embedded scheduler would sit idle
    # holding no informers at all — which would measure a standby, report an
    # impressively small number, and mean nothing. Turning it off makes this
    # instance always active.
    #
    # That also means this must not run while anything else is scheduling
    # against the same cluster; see the guard in start_upstream_kube_scheduler.
    cat > "$SCHED_WORK_DIR/config.yaml" <<EOF
apiVersion: kubescheduler.config.k8s.io/v1
kind: KubeSchedulerConfiguration
clientConnection:
  kubeconfig: $KUBECONFIG_PATH
leaderElection:
  leaderElect: false
EOF
}

write_scheduler_service() {
    cat > "/etc/systemd/system/$SERVICE_NAME" <<EOF
[Unit]
Description=Upstream kube-scheduler (profiling comparison rig — not a production deployment)
After=network.target k3s.service
Wants=k3s.service

[Service]
ExecStart=$SCHED_BIN --config=$SCHED_WORK_DIR/config.yaml --v=1
Restart=on-failure
RestartSec=2
KillMode=process

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
}

start_upstream_kube_scheduler() {
    # Two schedulers binding the same pods is a race, not a comparison. The
    # profiling legs are supposed to be run one at a time, and getting this
    # wrong would corrupt the cluster as well as the measurement — so it is
    # refused rather than warned about.
    if systemctl is-active --quiet nodescheduler 2>/dev/null; then
        echo "nodescheduler is running — stop it before starting the upstream rig, or both will bind the same pods." >&2
        return 1
    fi

    # k3s schedules from inside its own process (apiserver + scheduler +
    # controller-manager all one binary) unless started with
    # --disable-scheduler, so an "idle" nodescheduler tells us nothing about
    # whether k3s's own bundled scheduler is quietly still active. The flag
    # lives directly in the unit's baked-in ExecStart= (confirmed against a
    # real deployment: setup-control-plane.sh's INSTALL_K3S_EXEC becomes
    # literal `ExecStart=... '--disable-scheduler' \` lines, not a separate
    # config file) — `systemctl cat` is what reads that back.
    if command -v systemctl >/dev/null 2>&1 && systemctl cat k3s.service >/dev/null 2>&1; then
        if ! systemctl cat k3s.service 2>/dev/null | grep -q -- '--disable-scheduler'; then
            echo "k3s's own bundled scheduler is not disabled (--disable-scheduler absent from k3s.service's ExecStart) — restart k3s with SCHEDULER=nodescheduler (or otherwise pass --disable-scheduler) before starting this rig, or both will bind the same pods." >&2
            return 1
        fi
    fi

    install_upstream_kube_scheduler || return 1
    write_scheduler_config
    write_scheduler_service
    systemctl enable --now "$SERVICE_NAME"

    # Same shape as the kube-proxy rig: wait for it to be up AND still up. A
    # scheduler that exits two seconds in on a bad config would otherwise be
    # measured as a component with a wonderfully small footprint.
    local waited=0
    until systemctl is-active --quiet "$SERVICE_NAME"; do
        waited=$((waited + 2))
        if (( waited >= 60 )); then
            echo "kube-scheduler never came up within 60s — check 'journalctl -u $SERVICE_NAME'" >&2
            return 1
        fi
        sleep 2
    done
    sleep 5
    if ! systemctl is-active --quiet "$SERVICE_NAME"; then
        echo "kube-scheduler started and then exited — check 'journalctl -u $SERVICE_NAME'" >&2
        return 1
    fi

    # Informer caches fill on startup and dominate RSS, so a measurement taken
    # immediately would catch it mid-fill and understate it. The siblings have
    # the same property; this is the scheduler's equivalent settling period.
    log "kube-scheduler is running (pid $(systemctl show -p MainPID --value "$SERVICE_NAME")); letting its informer caches settle..."
    sleep 10
}

stop_upstream_kube_scheduler() {
    systemctl stop "$SERVICE_NAME" 2>/dev/null || true
    systemctl disable "$SERVICE_NAME" 2>/dev/null || true
    # Unlike kube-proxy there is nothing to clean up afterwards: a scheduler
    # writes Bindings and nothing else, and a Binding is a pod that is already
    # placed rather than a rule that keeps redirecting traffic once its author
    # is gone.
}

case "${1:-}" in
    start) start_upstream_kube_scheduler ;;
    stop) stop_upstream_kube_scheduler ;;
    *)
        echo "usage: $0 {start|stop}" >&2
        exit 2
        ;;
esac
