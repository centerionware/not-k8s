#!/usr/bin/env bash
# lib/upstream-kube-proxy.sh — install and run a REAL, standalone upstream
# kube-proxy against the same stripped k3s control plane nodeproxy uses, so a
# profiling run can measure kube-proxy's own idle RSS/CPU as a genuinely
# separate OS process.
#
# The sibling of lib/upstream-kubelet.sh, and for the same reason: a default
# k3s install runs kube-proxy as an embedded goroutine inside the same OS
# process as the apiserver/kine/controller-manager/scheduler, so there is no
# way to isolate "kube-proxy's own number" from a stock k3s process at the OS
# level. This gives it the exact footing nodeproxy already has — its own
# process, the same control plane, the same node.
#
# profiling.yml's own comment used to say this rig didn't exist, and that the
# published comparison therefore had to run --proxy=none on both legs to stay
# honest. This is that missing piece: with it, a run can compare the whole
# node side one-for-one (kubelet+kube-proxy+kine against
# nodelet+nodeproxy+nodestore) instead of only the node agent.
#
# This is a measurement rig, not a production posture: it reuses k3s's own
# admin kubeconfig directly rather than doing a real bootstrap, exactly as
# upstream-kubelet.sh does — see that file's header for why that is the
# comparison-fair choice here.
#
# iptables mode, deliberately: it is upstream's default and the mode a person
# actually gets. nodeproxy uses nftables, so this is not a like-for-like
# *implementation* comparison — it is a comparison of what each project ships
# by default, which is the honest question. Do not "fix" this by forcing
# kube-proxy into nftables mode to match; that would measure a configuration
# nobody runs, and on this project's own test kernel the nftables modules
# kube-proxy's nftables backend wants are missing anyway (see
# crates/nodeproxy/src/svc.rs's probe_caps()).
set -uo pipefail

PROXY_BIN=/usr/local/bin/kube-proxy
PROXY_WORK_DIR=/var/lib/upstream-kube-proxy
KUBECONFIG_PATH=/etc/rancher/k3s/k3s.yaml
SERVICE_NAME=upstream-kube-proxy.service
CLUSTER_CIDR="${CLUSTER_CIDR:-10.42.0.0/16}"

log() { echo "==> $*"; }

detect_arch() {
    case "$(uname -m)" in
        x86_64) echo amd64 ;;
        aarch64|arm64) echo arm64 ;;
        armv7l) echo arm ;;
        *) return 1 ;;
    esac
}

# Same rule as upstream-kubelet.sh: fetch the kube-proxy matching the real
# upstream Kubernetes version k3s embeds ("k3s version v1.31.4+k3s1" ->
# v1.31.4), so this is measured against the API generation this control plane
# actually speaks rather than an arbitrary release.
detect_k8s_version() {
    local v
    v="$(k3s --version 2>/dev/null | head -1 | awk '{print $3}')"
    [[ -n "$v" ]] || return 1
    echo "${v%%+*}"
}

install_upstream_kube_proxy() {
    if [[ -x "$PROXY_BIN" ]]; then
        log "kube-proxy already installed: $("$PROXY_BIN" --version)"
        return 0
    fi

    local arch k8s_version
    arch="$(detect_arch)" || { echo "unsupported architecture for upstream kube-proxy: $(uname -m)" >&2; return 1; }
    k8s_version="$(detect_k8s_version)" || { echo "couldn't determine k3s's embedded Kubernetes version from 'k3s --version'" >&2; return 1; }

    log "fetching real upstream kube-proxy $k8s_version ($arch) — matching this cluster's k3s-embedded Kubernetes version..."
    curl -sfL -o "$PROXY_BIN" "https://dl.k8s.io/release/${k8s_version}/bin/linux/${arch}/kube-proxy" \
        || { echo "failed to download kube-proxy $k8s_version for $arch from dl.k8s.io" >&2; return 1; }
    chmod +x "$PROXY_BIN"
    log "installed: $("$PROXY_BIN" --version)"
}

write_proxy_config() {
    mkdir -p "$PROXY_WORK_DIR"
    # A config file rather than a pile of CLI flags, matching how
    # upstream-kubelet.sh does it and how a real deployment configures
    # kube-proxy today.
    #
    # conntrack maxPerCore 0 leaves the kernel's own nf_conntrack_max alone:
    # kube-proxy otherwise raises it at startup based on the core count, which
    # is a one-off write that has nothing to do with steady-state cost but
    # will fail loudly on a host where that sysctl isn't writable.
    cat > "$PROXY_WORK_DIR/config.yaml" <<EOF
apiVersion: kubeproxy.config.k8s.io/v1alpha1
kind: KubeProxyConfiguration
clientConnection:
  kubeconfig: $KUBECONFIG_PATH
clusterCIDR: $CLUSTER_CIDR
hostnameOverride: $(uname -n)
mode: "iptables"
conntrack:
  maxPerCore: 0
  tcpEstablishedTimeout: 0s
  tcpCloseWaitTimeout: 0s
EOF
}

write_proxy_service() {
    cat > "/etc/systemd/system/$SERVICE_NAME" <<EOF
[Unit]
Description=Upstream kube-proxy (profiling comparison rig — not a production deployment)
After=network.target k3s.service
Wants=k3s.service

[Service]
ExecStart=$PROXY_BIN --config=$PROXY_WORK_DIR/config.yaml --v=1
Restart=on-failure
RestartSec=2
KillMode=process

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
}

start_upstream_kube_proxy() {
    install_upstream_kube_proxy || return 1
    write_proxy_config
    write_proxy_service
    systemctl enable --now "$SERVICE_NAME"

    # kube-proxy has no Node object to wait on the way kubelet does, so wait
    # on the process itself being up and still up — a kube-proxy that exits
    # two seconds in (bad config, missing kubeconfig) would otherwise be
    # measured as a component with a wonderfully small footprint.
    local waited=0
    until systemctl is-active --quiet "$SERVICE_NAME"; do
        waited=$((waited + 2))
        if (( waited >= 60 )); then
            echo "kube-proxy never came up within 60s — check 'journalctl -u $SERVICE_NAME'" >&2
            return 1
        fi
        sleep 2
    done
    sleep 5
    if ! systemctl is-active --quiet "$SERVICE_NAME"; then
        echo "kube-proxy started and then exited — check 'journalctl -u $SERVICE_NAME'" >&2
        return 1
    fi
    log "kube-proxy is running: $(systemctl show -p MainPID --value "$SERVICE_NAME")"
}

stop_upstream_kube_proxy() {
    systemctl stop "$SERVICE_NAME" 2>/dev/null || true
    systemctl disable "$SERVICE_NAME" 2>/dev/null || true
    # Leave the rules it wrote behind only as long as it takes to clean them:
    # a stale KUBE-SERVICES chain would otherwise still be redirecting traffic
    # during the *other* leg of a comparison, which is both a wrong
    # measurement and a confusing network.
    if command -v iptables >/dev/null 2>&1; then
        iptables -w 5 -t nat -F KUBE-SERVICES 2>/dev/null || true
        iptables -w 5 -t nat -F KUBE-POSTROUTING 2>/dev/null || true
        iptables -w 5 -t nat -F KUBE-NODEPORTS 2>/dev/null || true
    fi
}

case "${1:-}" in
    start) start_upstream_kube_proxy ;;
    stop) stop_upstream_kube_proxy ;;
    *)
        echo "usage: $0 {start|stop}" >&2
        exit 2
        ;;
esac
