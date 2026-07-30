# lib/nft.sh — Service proxy (ClusterIP/NodePort) host-side dependencies.
# nodelet itself watches Services/Endpoints and programs the rules
# (crates/nodelet/src/svc.rs); this just makes sure `nft` exists and that
# bridged pod traffic actually reaches the host's netfilter tables
# (br_netfilter), since without that a pod calling a ClusterIP never hits
# the DNAT rule at all.

ensure_nft() {
    [[ "$WITH_CRI" -eq 1 && "$CNI_PLUGIN" != "none" ]] || return 0
    command -v nft &>/dev/null && return 0
    pkg_install "nftables" "nftables" "nftables" "nftables" "nftables" "nftables" "nftables" || true
    command -v nft &>/dev/null \
        || warn "Could not get nftables — ClusterIP/NodePort routing will be unavailable. \
nodelet detects this and skips the Service proxy; direct pod-IP traffic is unaffected."
}

enable_bridge_netfilter() {
    [[ "$WITH_CRI" -eq 1 && "$CNI_PLUGIN" != "none" ]] || return 0
    modprobe br_netfilter 2>/dev/null || true
    sysctl -w net.ipv4.ip_forward=1 >/dev/null 2>&1 || true
    if [[ -f /proc/sys/net/bridge/bridge-nf-call-iptables ]]; then
        sysctl -w net.bridge.bridge-nf-call-iptables=1 >/dev/null 2>&1 || true
    else
        warn "net.bridge.bridge-nf-call-iptables isn't present (br_netfilter didn't load — common in \
nested/unprivileged containers) — pods calling a ClusterIP may not be DNAT'd. Traffic \
originated by the host itself still works."
    fi
}
