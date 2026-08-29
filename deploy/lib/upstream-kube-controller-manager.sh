# lib/upstream-kube-controller-manager.sh — install and run a REAL,
# standalone upstream kube-controller-manager against the real upstream
# kube-apiserver (see the sibling upstream-kube-apiserver.sh), as its own OS
# process. The second half of retiring k3s's embedded control plane
# entirely: node lifecycle (taints for NotReady/unreachable), ServiceAccount
# token/secret provisioning, and CSR signing all move from k3s's own
# in-process controller-manager goroutine to a real, separately-measurable
# binary.
#
# Reuses k3s's own admin client cert (client-admin.crt/key) the same way the
# other three upstream-*.sh rigs already do — see upstream-kubelet.sh's own
# header for why that is the comparison-fair choice here. k3s's *dedicated*
# tls/kube-controller-manager/kube-controller-manager.crt looks like the more
# correct choice at first (a real deployment would use exactly this
# separation) but it is not actually usable as a client certificate: openssl
# x509 -text on it (confirmed live) shows Extended Key Usage = TLS Web Server
# Authentication only, no Client Authentication — k3s's own embedded
# controller-manager evidently never exercises it over real client TLS, and
# a real standalone kube-apiserver's x509 authenticator correctly refuses it
# ("certificate specifies an incompatible key usage"). Also reuses the
# cluster CA and service-account key for the controllers that mint
# credentials (CSR signing, ServiceAccount tokens, the root-ca.crt every
# namespace gets via the cluster-info configmap).
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
        echo "one or more required PKI files are missing — this rig needs k3s's own TLS dir, generated on its first start (run bootstrap-source.sh at least once before retiring k3s)." >&2
        return 1
    fi
}

# A real kubeconfig, not a bare cert path — kube-controller-manager takes
# --kubeconfig, unlike --tls-cert-file-style flags. Points at the same
# apiserver upstream-kube-apiserver.sh just started, authenticating with
# k3s's own admin client cert (system:masters) — see this file's own header
# for why, after k3s's dedicated controller-manager cert turned out to be
# unusable as a client credential.
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
After=network.target upstream-kube-apiserver.service
Requires=upstream-kube-apiserver.service

[Service]
WorkingDirectory=$CM_WORK_DIR
ExecStart=$CM_BIN \\
    --kubeconfig=$CM_WORK_DIR/kubeconfig \\
    --authentication-kubeconfig=$CM_WORK_DIR/kubeconfig \\
    --authorization-kubeconfig=$CM_WORK_DIR/kubeconfig \\
    --authentication-tolerate-lookup-failure=true \\
    --leader-elect=true \\
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
    if systemctl is-active --quiet k3s 2>/dev/null; then
        echo "k3s is still running — stop it first (systemctl stop k3s), or two controller-managers will race each other for the same objects." >&2
        return 1
    fi
    if ! systemctl is-active --quiet upstream-kube-apiserver 2>/dev/null; then
        echo "upstream-kube-apiserver isn't running — start it first (deploy/lib/upstream-kube-apiserver.sh start)." >&2
        return 1
    fi

    check_pki_files || return 1
    install_upstream_kube_controller_manager || return 1
    write_cm_kubeconfig
    write_cm_service
    systemctl enable --now "$SERVICE_NAME"

    log "waiting for it to win leader election..."
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
    log "kube-controller-manager is running (pid $(systemctl show -p MainPID --value "$SERVICE_NAME"))."
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
