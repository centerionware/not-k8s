# lib/upstream-kube-apiserver.sh — install and run a REAL, standalone
# upstream kube-apiserver against nodestore, so the LAST two pieces of the
# control plane k3s still provides (this and kube-controller-manager, see the
# sibling script) can be measured and run as genuinely separate OS processes
# too — completing the same substitution nodelet/nodeproxy/nodescheduler/
# nodestore already did for kubelet/kube-proxy/kube-scheduler/etcd.
#
# Deliberately reuses k3s's own already-generated PKI
# (/var/lib/rancher/k3s/server/tls/) rather than minting a fresh one: those
# files are what /etc/rancher/k3s/k3s.yaml's client cert was issued against,
# what nodelet already trusts for kubelet-certificate-authority, and what the
# serving cert's SANs (kubernetes.default.svc, 127.0.0.1, this node's own
# hostname/IP) already match — reusing them is what lets every existing
# client (kubectl, nodelet, nodeproxy, nodescheduler) keep working against
# the real apiserver with zero reconfiguration. This is a measurement rig,
# not a production posture: a real deployment would run its own CA, not
# borrow one out of a stripped k3s install being retired.
#
# nodestore, not k3s's bundled kine: this only makes sense to run once the
# control plane is already on nodestore (DATASTORE=nodestore) — see
# bootstrap-source.sh. kine is embedded inside k3s's own process and stops
# existing the moment k3s does; nodestore is nodestore-service.sh's genuinely
# standalone etcd v3 server, the one piece of this rig that predates it.
set -uo pipefail

APISERVER_BIN=/usr/local/bin/kube-apiserver
APISERVER_WORK_DIR=/var/lib/upstream-kube-apiserver
K3S_TLS_DIR=/var/lib/rancher/k3s/server/tls
NODESTORE_PKI_DIR="${NODESTORE_DATA_DIR:-/var/lib/nodestore}/pki/client"
SERVICE_NAME=upstream-kube-apiserver.service
SERVICE_CIDR="${SERVICE_CIDR:-10.43.0.0/16}"
SERVICE_ACCOUNT_ISSUER="${SERVICE_ACCOUNT_ISSUER:-https://kubernetes.default.svc.cluster.local}"

log() { echo "==> $*"; }

detect_arch() {
    case "$(uname -m)" in
        x86_64) echo amd64 ;;
        aarch64|arm64) echo arm64 ;;
        armv7l) echo arm ;;
        *) return 1 ;;
    esac
}

# Same rule as the other three upstream-*.sh rigs: fetch the kube-apiserver
# matching the real upstream Kubernetes version k3s embeds ("k3s version
# v1.31.4+k3s1" -> v1.31.4), so this is measured against — and interoperates
# with — the exact API generation the rest of this cluster (nodelet,
# nodeproxy, nodescheduler, kubectl) was already speaking to.
detect_k8s_version() {
    local v
    v="$(k3s --version 2>/dev/null | head -1 | awk '{print $3}')"
    [[ -n "$v" ]] || return 1
    echo "${v%%+*}"
}

install_upstream_kube_apiserver() {
    if [[ -x "$APISERVER_BIN" ]]; then
        log "kube-apiserver already installed: $("$APISERVER_BIN" --version)"
        return 0
    fi

    local arch k8s_version
    arch="$(detect_arch)" || { echo "unsupported architecture for upstream kube-apiserver: $(uname -m)" >&2; return 1; }
    k8s_version="$(detect_k8s_version)" || { echo "couldn't determine k3s's embedded Kubernetes version from 'k3s --version'" >&2; return 1; }

    log "fetching real upstream kube-apiserver $k8s_version ($arch) — matching this cluster's k3s-embedded Kubernetes version..."
    curl -sfL -o "$APISERVER_BIN" "https://dl.k8s.io/release/${k8s_version}/bin/linux/${arch}/kube-apiserver" \
        || { echo "failed to download kube-apiserver $k8s_version for $arch from dl.k8s.io" >&2; return 1; }
    chmod +x "$APISERVER_BIN"
    log "installed: $("$APISERVER_BIN" --version)"
}

# Every file this rig borrows from k3s's own PKI and nodestore's own client
# cert, checked up front — a missing one otherwise surfaces as an opaque TLS
# handshake failure deep in the unit's own log instead of a clear "which file
# and why" here.
check_pki_files() {
    local f missing=0
    for f in \
        "$K3S_TLS_DIR/server-ca.crt" \
        "$K3S_TLS_DIR/client-ca.crt" \
        "$K3S_TLS_DIR/serving-kube-apiserver.crt" "$K3S_TLS_DIR/serving-kube-apiserver.key" \
        "$K3S_TLS_DIR/client-kube-apiserver.crt" "$K3S_TLS_DIR/client-kube-apiserver.key" \
        "$K3S_TLS_DIR/service.key" \
        "$NODESTORE_PKI_DIR/ca.crt" "$NODESTORE_PKI_DIR/client.crt" "$NODESTORE_PKI_DIR/client.key"
    do
        if [[ ! -s "$f" ]]; then
            echo "missing or empty: $f" >&2
            missing=1
        fi
    done
    if [[ "$missing" -eq 1 ]]; then
        echo "one or more required PKI files are missing — this rig needs k3s to have run at least once (its TLS dir is generated on first start) with DATASTORE=nodestore (nodestore's own client PKI is generated on nodestore's first start)." >&2
        return 1
    fi
}

# k3s issues its client certificates from two different CAs, not one: admin/
# kube-apiserver/kube-scheduler are signed by client-ca, but (confirmed live
# — openssl verify against each) kube-controller-manager's own dedicated cert
# is signed by server-ca instead. A --client-ca-file naming only client-ca
# therefore authenticates every one of k3s's other client certs but rejects
# kube-controller-manager's with a bare "Unauthorized" (not "Forbidden" — it
# never gets far enough to reach RBAC). kube-apiserver's cert-pool loader
# accepts any number of concatenated PEM blocks in one file, so the fix is a
# combined bundle rather than picking one CA and reissuing the other cert.
write_client_ca_bundle() {
    cat "$K3S_TLS_DIR/client-ca.crt" "$K3S_TLS_DIR/server-ca.crt" > "$APISERVER_WORK_DIR/client-ca-bundle.crt"
}

# --advertise-address is what populates the default/kubernetes Service's own
# Endpoints (round-tripped through the apiserver's own endpoint reconciler)
# — left unset, kube-apiserver auto-detects the node's external-facing IP,
# which is real but not reliably reachable from *inside* a pod's network
# namespace back out through the host's own external interface (found live
# here: coredns's in-cluster client, using exactly that env-var/DNS-derived
# Service address, got "network is unreachable" hairpinning back to it —
# nodelet/nodeproxy/nodescheduler never hit this because they dial
# 127.0.0.1:6443 directly via k3s.yaml, not through the Service). Loopback
# looks like the fix but isn't: the apiserver's own endpoint reconciler
# rejects it outright ("may not be in the loopback range", confirmed live —
# it never stops retrying and logging the same error, and the Endpoints
# object simply stays empty forever). What actually works is the CNI
# bridge's own gateway address — every pod's default route goes through it,
# so it's reachable from any pod's netns exactly like loopback would be if
# it were allowed, without tripping that validation.
detect_advertise_address() {
    local ip
    ip="$(ip -4 -o addr show cni0 2>/dev/null | awk '{print $4}' | cut -d/ -f1)"
    echo "${ip:-127.0.0.1}"
}

write_apiserver_service() {
    local advertise_address
    advertise_address="$(detect_advertise_address)"
    cat > "/etc/systemd/system/$SERVICE_NAME" <<EOF
[Unit]
Description=Upstream kube-apiserver (profiling comparison rig — not a production deployment)
After=network.target nodestore.service
Requires=nodestore.service

[Service]
WorkingDirectory=$APISERVER_WORK_DIR
ExecStart=$APISERVER_BIN \\
    --etcd-servers=https://127.0.0.1:2379 \\
    --etcd-cafile=$NODESTORE_PKI_DIR/ca.crt \\
    --etcd-certfile=$NODESTORE_PKI_DIR/client.crt \\
    --etcd-keyfile=$NODESTORE_PKI_DIR/client.key \\
    --secure-port=6443 \\
    --bind-address=0.0.0.0 \\
    --advertise-address=$advertise_address \\
    --tls-cert-file=$K3S_TLS_DIR/serving-kube-apiserver.crt \\
    --tls-private-key-file=$K3S_TLS_DIR/serving-kube-apiserver.key \\
    --client-ca-file=$APISERVER_WORK_DIR/client-ca-bundle.crt \\
    --kubelet-client-certificate=$K3S_TLS_DIR/client-kube-apiserver.crt \\
    --kubelet-client-key=$K3S_TLS_DIR/client-kube-apiserver.key \\
    --kubelet-certificate-authority=/var/lib/nodelet/pki/server-ca.pem \\
    --kubelet-preferred-address-types=InternalIP,Hostname,ExternalIP \\
    --service-account-key-file=$K3S_TLS_DIR/service.key \\
    --service-account-signing-key-file=$K3S_TLS_DIR/service.key \\
    --service-account-issuer=$SERVICE_ACCOUNT_ISSUER \\
    --service-cluster-ip-range=$SERVICE_CIDR \\
    --authorization-mode=Node,RBAC \\
    --enable-admission-plugins=NodeRestriction \\
    --allow-privileged=true \\
    --anonymous-auth=false \\
    --feature-gates=ResourceHealthStatus=true,ServiceAccountTokenPodNodeInfo=true \\
    --v=1
Restart=on-failure
RestartSec=2
KillMode=process

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
}

start_upstream_kube_apiserver() {
    if systemctl is-active --quiet k3s 2>/dev/null; then
        echo "k3s is still running — stop it first (systemctl stop k3s), or two apiservers will both be writing to nodestore at once." >&2
        return 1
    fi

    check_pki_files || return 1
    install_upstream_kube_apiserver || return 1
    mkdir -p "$APISERVER_WORK_DIR"
    write_client_ca_bundle
    write_apiserver_service
    systemctl enable --now "$SERVICE_NAME"

    log "waiting for the apiserver to answer /readyz..."
    local waited=0
    # --anonymous-auth=false above means /readyz genuinely requires a client
    # cert, not just a trusted CA — an anonymous request gets a real 401, not
    # a "not ready yet". k3s's own admin client cert authenticates as
    # system:masters, which every RBAC-enabled apiserver always lets through.
    until curl -sfk --cacert "$K3S_TLS_DIR/server-ca.crt" \
        --cert "$K3S_TLS_DIR/client-admin.crt" --key "$K3S_TLS_DIR/client-admin.key" \
        https://127.0.0.1:6443/readyz >/dev/null 2>&1; do
        waited=$((waited + 2))
        if (( waited >= 60 )); then
            echo "kube-apiserver never became ready within 60s — check 'journalctl -u $SERVICE_NAME'" >&2
            return 1
        fi
        sleep 2
    done
    log "kube-apiserver is ready (waited ${waited}s; pid $(systemctl show -p MainPID --value "$SERVICE_NAME"))."
}

stop_upstream_kube_apiserver() {
    systemctl stop "$SERVICE_NAME" 2>/dev/null || true
    systemctl disable "$SERVICE_NAME" 2>/dev/null || true
}

case "${1:-}" in
    start) start_upstream_kube_apiserver ;;
    stop) stop_upstream_kube_apiserver ;;
    *)
        echo "usage: $0 {start|stop}" >&2
        exit 2
        ;;
esac
