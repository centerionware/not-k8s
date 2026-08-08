#!/usr/bin/env bash
# lib/upstream-kubelet.sh — install and run a REAL, standalone upstream
# kubelet binary against the same stripped k3s control plane nodelet uses,
# so profiling.yml can measure kubelet's own idle RSS/CPU as a genuinely
# separate OS process. A default k3s install can't be used for this
# comparison at all: it runs kubelet as an embedded goroutine inside the
# same OS process as the apiserver/etcd/controller-manager/scheduler (see
# measure.sh's own doc comment), so there is no way to isolate "kubelet's
# own number" from a stock k3s process at the OS-process level. This
# script gives kubelet the exact same footing nodelet already has: its own
# process, the same control plane, the same containerd.
#
# This is a measurement rig, not a production posture: it reuses k3s's own
# admin kubeconfig (/etc/rancher/k3s/k3s.yaml) directly as kubelet's
# --kubeconfig rather than performing a real TLS bootstrap (matching how
# nodelet itself already authenticates by default in this project's own
# dev/measurement setups -- see crates/nodelet/src/main.rs's own comment:
# TLS bootstrap is opt-in via NODELET_BOOTSTRAP_KUBECONFIG, off unless
# set), and it does not run kube-proxy or a CNI (not needed to reach
# kubelet's own genuine idle steady-state -- no pods are scheduled here,
# only the same "join the cluster and sit idle" state nodelet is measured
# in on the other side of the comparison).
set -uo pipefail

KUBELET_BIN=/usr/local/bin/kubelet
KUBELET_WORK_DIR=/var/lib/upstream-kubelet
KUBECONFIG_PATH=/etc/rancher/k3s/k3s.yaml
SERVICE_NAME=upstream-kubelet.service

log() { echo "==> $*"; }

detect_arch() {
    case "$(uname -m)" in
        x86_64) echo amd64 ;;
        aarch64|arm64) echo arm64 ;;
        armv7l) echo arm ;;
        *) return 1 ;;
    esac
}

# The Kubernetes version to fetch a matching kubelet for -- k3s's own
# --version output looks like "k3s version v1.31.4+k3s1 (abcdef01)"; the
# part before "+k3s1" is the real upstream Kubernetes version k3s embeds,
# so fetching that exact version's kubelet keeps this an honest comparison
# against the same API generation this control plane actually speaks,
# rather than an arbitrary/possibly-skewed kubelet release.
detect_k8s_version() {
    local v
    v="$(k3s --version 2>/dev/null | head -1 | awk '{print $3}')"
    [[ -n "$v" ]] || return 1
    echo "${v%%+*}"
}

install_upstream_kubelet() {
    if [[ -x "$KUBELET_BIN" ]]; then
        log "kubelet already installed: $("$KUBELET_BIN" --version)"
        return 0
    fi

    local arch k8s_version
    arch="$(detect_arch)" || { echo "unsupported architecture for upstream kubelet: $(uname -m)" >&2; return 1; }
    k8s_version="$(detect_k8s_version)" || { echo "couldn't determine k3s's embedded Kubernetes version from 'k3s --version'" >&2; return 1; }

    log "fetching real upstream kubelet $k8s_version ($arch) — matching this cluster's k3s-embedded Kubernetes version..."
    curl -sfL -o "$KUBELET_BIN" "https://dl.k8s.io/release/${k8s_version}/bin/linux/${arch}/kubelet" \
        || { echo "failed to download kubelet $k8s_version for $arch from dl.k8s.io" >&2; return 1; }
    chmod +x "$KUBELET_BIN"
    log "installed: $("$KUBELET_BIN" --version)"
}

write_kubelet_config() {
    mkdir -p "$KUBELET_WORK_DIR"
    # Modern KubeletConfiguration (not a pile of deprecated CLI flags —
    # --network-plugin/--pod-infra-container-image and friends are gone in
    # current Kubernetes). cgroupDriver: systemd matches containerd's own
    # default SystemdCgroup wiring this project's container-runtime.sh
    # already sets up for nodelet's own CRI path, so both node agents run
    # under the identical cgroup topology.
    cat > "$KUBELET_WORK_DIR/config.yaml" <<EOF
apiVersion: kubelet.config.k8s.io/v1beta1
kind: KubeletConfiguration
address: 0.0.0.0
authentication:
  anonymous:
    enabled: false
  webhook:
    enabled: true
authorization:
  mode: Webhook
cgroupDriver: systemd
containerRuntimeEndpoint: unix:///run/containerd/containerd.sock
failSwapOn: false
readOnlyPort: 0
serializeImagePulls: false
staticPodPath: ""
EOF
}

write_kubelet_service() {
    local node_ip
    node_ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
    cat > "/etc/systemd/system/$SERVICE_NAME" <<EOF
[Unit]
Description=Upstream kubelet (profiling comparison rig — not a production deployment)
After=network.target k3s.service containerd.service
Requires=containerd.service

[Service]
ExecStart=$KUBELET_BIN \\
    --kubeconfig=$KUBECONFIG_PATH \\
    --config=$KUBELET_WORK_DIR/config.yaml \\
    --root-dir=$KUBELET_WORK_DIR \\
    --hostname-override=$(hostname) \\
    --node-ip=$node_ip \\
    --v=1
Restart=on-failure
RestartSec=2
KillMode=process

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
}

start_upstream_kubelet() {
    install_upstream_kubelet || return 1
    write_kubelet_config
    write_kubelet_service

    # A stale Node object from a previous phase (e.g. nodelet, registered
    # under the same hostname just before this) would otherwise leave
    # kubelet updating pre-existing status/conditions/capacity instead of
    # a genuinely fresh registration — delete it first so both sides of
    # the comparison start from the same clean slate.
    KUBECONFIG="$KUBECONFIG_PATH" kubectl delete node "$(hostname)" --ignore-not-found --wait=true >/dev/null 2>&1 || true

    systemctl enable --now "$SERVICE_NAME"

    log "waiting for the node to register..."
    local waited=0
    until KUBECONFIG="$KUBECONFIG_PATH" kubectl get node "$(hostname)" >/dev/null 2>&1; do
        waited=$((waited + 2))
        if (( waited >= 60 )); then
            echo "kubelet never registered a Node within 60s — check 'journalctl -u $SERVICE_NAME'" >&2
            return 1
        fi
        sleep 2
    done
    KUBECONFIG="$KUBECONFIG_PATH" kubectl get node "$(hostname)" -o wide || true
}

stop_upstream_kubelet() {
    systemctl stop "$SERVICE_NAME" 2>/dev/null || true
    systemctl disable "$SERVICE_NAME" 2>/dev/null || true
    KUBECONFIG="$KUBECONFIG_PATH" kubectl delete node "$(hostname)" --ignore-not-found --wait=true >/dev/null 2>&1 || true
}

case "${1:-}" in
    start) start_upstream_kubelet ;;
    stop) stop_upstream_kubelet ;;
    *)
        echo "usage: $0 {start|stop}" >&2
        exit 2
        ;;
esac
