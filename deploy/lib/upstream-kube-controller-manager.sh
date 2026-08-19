# lib/upstream-kube-controller-manager.sh — install and run a REAL,
# standalone upstream kube-controller-manager against the same stripped k3s
# control plane nodecontroller uses, so a profiling run can measure
# kube-controller-manager's own idle RSS/CPU as a genuinely separate OS
# process.
#
# The fourth sibling of lib/upstream-kubelet.sh, lib/upstream-kube-proxy.sh
# and lib/upstream-kube-scheduler.sh, and for exactly the same reason: k3s
# runs the controller-manager as an embedded goroutine inside the same OS
# process as the apiserver and scheduler, so there is no way to isolate "the
# controller-manager's own number" from a stock k3s process at the OS level.
# This gives it the footing nodecontroller already has — its own process,
# the same control plane, the same node.
#
# History: an earlier version of this file (branch
# upstream-kube-apiserver-controller-manager, predating nodecontroller's
# --controller-manager=nodecontroller / k3s's --disable-controller-manager
# flag) took the only option available at the time — stop k3s entirely and
# stand up a separate real upstream-kube-apiserver.sh alongside it. Now that
# --disable-controller-manager exists (the exact analog of
# --disable-scheduler), this rig no longer needs that: it points straight
# at k3s's own still-running apiserver on 127.0.0.1:6443, the same as
# upstream-kube-scheduler.sh does, and only k3s's *bundled* controller-manager
# goroutine needs to be off — not k3s itself.
#
# Reuses k3s's own admin client cert (client-admin.crt/key) as the client
# credential — see upstream-kubelet.sh's own header for why that is the
# comparison-fair choice here. k3s's *dedicated*
# tls/kube-controller-manager/kube-controller-manager.crt looks like the more
# correct choice at first (a real deployment would use exactly this
# separation) but it is not actually usable as a client certificate: openssl
# x509 -text on it (confirmed live) shows Extended Key Usage = TLS Web Server
# Authentication only, no Client Authentication — k3s's own embedded
# controller-manager evidently never exercises it over real client TLS, and a
# real standalone kube-apiserver's x509 authenticator correctly refuses it
# ("certificate specifies an incompatible key usage"). Also reuses the
# cluster CA and service-account key for the controllers that mint
# credentials (CSR signing, ServiceAccount tokens, the root-ca.crt every
# namespace gets via the cluster-info configmap) — all of it k3s's own,
# generated once on first start, unrelated to which controller-manager is
# currently active.
#
# This is a measurement rig, not a production posture: it reuses k3s's own
# admin kubeconfig rather than issuing a controller-manager-specific
# credential, which no real deployment should do.
set -uo pipefail

CM_BIN=/usr/local/bin/kube-controller-manager
CM_WORK_DIR=/var/lib/upstream-kube-controller-manager
K3S_TLS_DIR=/var/lib/rancher/k3s/server/tls
APISERVER_CA="$K3S_TLS_DIR/server-ca.crt"
SERVICE_NAME=upstream-kube-controller-manager.service
CLUSTER_CIDR="${CLUSTER_CIDR:-10.42.0.0/16}"
NODE_CIDR_MASK_SIZE="${NODE_CIDR_MASK_SIZE:-24}"

log() { echo "==> $*"; }

detect_arch() {
    case "$(uname -m)" in
        x86_64) echo amd64 ;;
        aarch64|arm64) echo arm64 ;;
        armv7l) echo arm ;;
        *) return 1 ;;
    esac
}

# Same rule as its siblings — see upstream-kubelet.sh's own comment for why
# this has to be the version k3s embedded, not an arbitrary release.
detect_k8s_version() {
    local v
    v="$(k3s --version 2>/dev/null | head -1 | awk '{print $3}')"
    [[ -n "$v" ]] || return 1
    echo "${v%%+*}"
}

install_upstream_kube_controller_manager() {
    if [[ -x "$CM_BIN" ]]; then
        log "kube-controller-manager already installed: $("$CM_BIN" --version)"
        return 0
    fi

    local arch k8s_version
    arch="$(detect_arch)" || { echo "unsupported architecture for upstream kube-controller-manager: $(uname -m)" >&2; return 1; }
    k8s_version="$(detect_k8s_version)" || { echo "couldn't determine k3s's embedded Kubernetes version from 'k3s --version'" >&2; return 1; }

    log "fetching real upstream kube-controller-manager $k8s_version ($arch) — matching this cluster's k3s-embedded Kubernetes version..."
    curl -sfL -o "$CM_BIN" "https://dl.k8s.io/release/${k8s_version}/bin/linux/${arch}/kube-controller-manager" \
        || { echo "failed to download kube-controller-manager $k8s_version for $arch from dl.k8s.io" >&2; return 1; }
    chmod +x "$CM_BIN"
    log "installed: $("$CM_BIN" --version)"
}

check_pki_files() {
    local f missing=0
    for f in \
        "$APISERVER_CA" "$K3S_TLS_DIR/server-ca.key" \
        "$K3S_TLS_DIR/service.key" \
        "$K3S_TLS_DIR/client-admin.crt" "$K3S_TLS_DIR/client-admin.key"
    do
        if [[ ! -s "$f" ]]; then
            echo "missing or empty: $f" >&2
            missing=1
        fi
    done
    if [[ "$missing" -eq 1 ]]; then
        echo "one or more required PKI files are missing — this rig needs k3s's own TLS dir, generated on its first start (run bootstrap-source.sh at least once before using this rig)." >&2
        return 1
    fi
}

# A real kubeconfig, not a bare cert path — kube-controller-manager takes
# --kubeconfig, unlike --tls-cert-file-style flags. Points at k3s's own
# still-running apiserver on 127.0.0.1:6443 (the same port whether k3s's
# bundled controller-manager, nodecontroller, or this rig is the one
# actually doing the writing), authenticating with k3s's own admin client
# cert (system:masters) — see this file's own header for why, after k3s's
# dedicated controller-manager cert turned out to be unusable as a client
# credential.
write_cm_kubeconfig() {
    mkdir -p "$CM_WORK_DIR"
    cat > "$CM_WORK_DIR/kubeconfig" <<EOF
apiVersion: v1
kind: Config
clusters:
- name: default
  cluster:
    server: https://127.0.0.1:6443
    certificate-authority: $APISERVER_CA
contexts:
- name: default
  context:
    cluster: default
    user: kube-controller-manager
current-context: default
users:
- name: kube-controller-manager
  user:
    client-certificate: $K3S_TLS_DIR/client-admin.crt
    client-key: $K3S_TLS_DIR/client-admin.key
EOF
}

write_cm_service() {
    cat > "/etc/systemd/system/$SERVICE_NAME" <<EOF
[Unit]
Description=Upstream kube-controller-manager (profiling comparison rig — not a production deployment)
After=network.target k3s.service
Wants=k3s.service

[Service]
WorkingDirectory=$CM_WORK_DIR
ExecStart=$CM_BIN \\
    --kubeconfig=$CM_WORK_DIR/kubeconfig \\
    --authentication-kubeconfig=$CM_WORK_DIR/kubeconfig \\
    --authorization-kubeconfig=$CM_WORK_DIR/kubeconfig \\
    --authentication-tolerate-lookup-failure=true \\
    --leader-elect=false \\
    --use-service-account-credentials=true \\
    --service-account-private-key-file=$K3S_TLS_DIR/service.key \\
    --root-ca-file=$APISERVER_CA \\
    --cluster-signing-cert-file=$APISERVER_CA \\
    --cluster-signing-key-file=$K3S_TLS_DIR/server-ca.key \\
    --allocate-node-cidrs=true \\
    --cluster-cidr=$CLUSTER_CIDR \\
    --node-cidr-mask-size=$NODE_CIDR_MASK_SIZE \\
    --node-monitor-period=10s \\
    --bind-address=127.0.0.1 \\
    --v=1
Restart=on-failure
RestartSec=2
KillMode=process

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
}

start_upstream_kube_controller_manager() {
    # Two controller-managers writing the same objects (node lifecycle
    # taints, podCIDR allocation, ServiceAccount/CSR issuance) is a race,
    # not a comparison — same shape as upstream-kube-scheduler.sh's guard,
    # for the same reason. The profiling legs are supposed to be run one at
    # a time.
    if systemctl is-active --quiet nodecontroller 2>/dev/null; then
        echo "nodecontroller is running — stop it before starting the upstream rig, or both will bind the same objects." >&2
        return 1
    fi

    # k3s runs its own controller-manager in-process (apiserver + scheduler
    # + controller-manager all one binary) unless started with
    # --disable-controller-manager, so an "idle" nodecontroller tells us
    # nothing about whether k3s's own bundled controller-manager is quietly
    # still active. The flag lives directly in the unit's baked-in
    # ExecStart= (same mechanism upstream-kube-scheduler.sh already relies
    # on for --disable-scheduler) — `systemctl cat` is what reads that back.
    if command -v systemctl >/dev/null 2>&1 && systemctl cat k3s.service >/dev/null 2>&1; then
        if ! systemctl cat k3s.service 2>/dev/null | grep -q -- '--disable-controller-manager'; then
            echo "k3s's own bundled controller-manager is not disabled (--disable-controller-manager absent from k3s.service's ExecStart) — restart k3s with CONTROLLER_MANAGER=nodecontroller (or otherwise pass --disable-controller-manager) before starting this rig, or both will bind the same objects." >&2
            return 1
        fi
    fi
    if ! systemctl is-active --quiet k3s 2>/dev/null; then
        echo "k3s isn't running — this rig points at k3s's own apiserver on 127.0.0.1:6443 (start k3s first)." >&2
        return 1
    fi

    check_pki_files || return 1
    install_upstream_kube_controller_manager || return 1
    write_cm_kubeconfig
    write_cm_service
    systemctl enable --now "$SERVICE_NAME"

    local waited=0
    until systemctl is-active --quiet "$SERVICE_NAME"; do
        waited=$((waited + 2))
        if (( waited >= 60 )); then
            echo "kube-controller-manager never came up within 60s — check 'journalctl -u $SERVICE_NAME'" >&2
            return 1
        fi
        sleep 2
    done
    sleep 5
    if ! systemctl is-active --quiet "$SERVICE_NAME"; then
        echo "kube-controller-manager started and then exited — check 'journalctl -u $SERVICE_NAME'" >&2
        return 1
    fi

    # Informer caches fill on startup and dominate RSS, so a measurement
    # taken immediately would catch it mid-fill and understate it. Same
    # settling period as upstream-kube-scheduler.sh.
    log "kube-controller-manager is running (pid $(systemctl show -p MainPID --value "$SERVICE_NAME")); letting its informer caches settle..."
    sleep 10
}

stop_upstream_kube_controller_manager() {
    systemctl stop "$SERVICE_NAME" 2>/dev/null || true
    systemctl disable "$SERVICE_NAME" 2>/dev/null || true
}

case "${1:-}" in
    start) start_upstream_kube_controller_manager ;;
    stop) stop_upstream_kube_controller_manager ;;
    *)
        echo "usage: $0 {start|stop}" >&2
        exit 2
        ;;
esac
