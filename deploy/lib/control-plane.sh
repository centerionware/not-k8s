# lib/control-plane.sh — the k3s control plane. k3s upstream only publishes
# binaries for amd64/arm64/armhf/s390x. That's a real, current limitation of
# using k3s as the control plane on truly exotic hardware — not something a
# shell script can paper over without building the whole of upstream
# Kubernetes + etcd/kine from source (hours, many GB, no guarantee of
# success on an untested arch). We detect and say so rather than pretend.

k3s_supports_arch() {
    case "$ARCH" in
        x86_64|aarch64|armv7l|s390x) return 0 ;;
        *) return 1 ;;
    esac
}

setup_control_plane() {
    [[ "$SKIP_CONTROL_PLANE" -eq 1 ]] && { log "Skipping control plane (--skip-control-plane)."; return 0; }

    if ! k3s_supports_arch; then
        warn "k3s has no upstream release for arch '$ARCH'. Known limitation — see README."
        warn "Skipping automatic control-plane setup. Run k3s from source or use k0s/another"
        warn "distro's build for this arch, then point KUBECONFIG at it and re-run with"
        warn "--skip-control-plane."
        return 0
    fi

    if command -v k3s &>/dev/null && systemctl is-active --quiet k3s 2>/dev/null; then
        log "k3s already installed and running."
    else
        "$SCRIPT_DIR/setup-control-plane.sh"
    fi
    export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
}

# enable_kubelet_certificate_authority_trust — wires the apiserver up to
# trust nodelet's self-signed kubelet-style server cert, so containerLogs/
# exec/attach/port-forward proxying actually works (see setup-control-
# plane.sh's own comment on --kube-apiserver-arg=kubelet-certificate-
# authority for why this, not a real CA, is what gets trusted here).
#
# Deliberately NOT part of setup_control_plane() / the first
# setup-control-plane.sh call: that runs before nodelet has ever started,
# and nodelet is what generates the cert file this needs to point the
# apiserver at (server::tls::load_or_generate(), triggered by nodelet's own
# normal startup — no special priming mode needed, it always writes
# server-ca.pem the same way whether this is watching for it or not). k3s
# starting before that file exists isn't fatal on its own (kube-apiserver
# just runs without kubelet-cert trust configured, same as before this
# function ever ran) — but the flag can't be added until the file is
# actually there, hence this being a *second*, later call into
# setup-control-plane.sh (its own installer is idempotent/safe to re-run —
# see its header comment) once nodelet's had a chance to start.
#
# Only meaningful for --with-cri (the mock runtime never starts the
# kubelet-style server at all — config.rs's server_enabled default).
# Best-effort: if the cert never shows up in time, warn and move on rather
# than fail the whole deployment over a proxying convenience nothing else
# here depends on.
enable_kubelet_certificate_authority_trust() {
    [[ "$SKIP_CONTROL_PLANE" -eq 1 || "$WITH_CRI" -ne 1 ]] && return 0
    k3s_supports_arch || return 0

    local cert_dir="${NODELET_SERVER_CERT_DIR:-/var/lib/nodelet/pki}"
    local pem_path="$cert_dir/server-ca.pem"

    log "Waiting for nodelet to generate its TLS cert (so the apiserver can be told to trust it for exec/logs/attach/port-forward)..."
    local waited=0 max_wait=30
    until [[ -s "$pem_path" ]]; do
        if (( waited >= max_wait )); then
            warn "nodelet never wrote $pem_path within ${max_wait}s — skipping apiserver kubelet-cert trust setup. 'kubectl exec/logs/attach/port-forward' will fail with a TLS trust error until this is done manually (see setup-control-plane.sh's header for the flag, and re-run this function once $pem_path exists)."
            return 0
        fi
        sleep 2
        waited=$((waited + 2))
    done

    log "Reconfiguring k3s to trust nodelet's TLS cert ($pem_path)..."
    NOTK8S_KUBELET_CA_PEM="$pem_path" "$SCRIPT_DIR/setup-control-plane.sh"
}
