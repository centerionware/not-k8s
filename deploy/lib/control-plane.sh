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
