#!/usr/bin/env bash
# run-flanneld.sh — wrapper that (re)writes flannel's net-conf.json on every
# single start, then execs flanneld.
#
# Why this exists instead of just writing net-conf.json once during install:
# a systemd unit with Restart=always only re-runs its ExecStart command —
# it never re-runs the installer script that originally wrote supporting
# config files. If /etc/kube-flannel/net-conf.json is ever missing when
# flanneld starts (fresh boot on a machine whose root filesystem doesn't
# persist /etc across reboots, a snapshot/golden-image reset, or just
# someone deleting it), flanneld crash-loops forever with "failed to read
# net conf: ... no such file or directory" and nothing ever fixes it short
# of manually re-running the whole install script. Writing the file fresh
# on every process start makes the service self-healing regardless of why
# the file went missing — confirmed for real: this is exactly the failure
# mode a live test machine hit after a reboot.
#
# Env vars (all provided by bootstrap-source.sh's systemd/OpenRC unit):
#   FLANNELD_BIN          absolute path to the flanneld binary (required —
#                         service managers give this script a minimal PATH,
#                         so `command -v flanneld` can't be trusted here)
#   KUBECONFIG            path to the kubeconfig flanneld's kube-subnet-mgr uses
#   IP_FAMILY             ipv4 | ipv6 | dual (default: ipv4)
#   IPV4_CLUSTER_CIDR     default: 10.42.0.0/16
#   IPV6_CLUSTER_CIDR     default: fd00:42::/56
#   FLANNEL_IFACE         optional: interface to run the overlay on. Defaults
#                         to whatever the default route names (see below) —
#                         set it on a multi-NIC host that wants a different
#                         one.
set -euo pipefail

: "${FLANNELD_BIN:?FLANNELD_BIN must be set to an absolute path to the flanneld binary}"
: "${KUBECONFIG:?KUBECONFIG must be set}"
IP_FAMILY="${IP_FAMILY:-ipv4}"
IPV4_CLUSTER_CIDR="${IPV4_CLUSTER_CIDR:-10.42.0.0/16}"
IPV6_CLUSTER_CIDR="${IPV6_CLUSTER_CIDR:-fd00:42::/56}"

mkdir -p /etc/kube-flannel /run/flannel

# nodecontroller owns PodCIDR allocation and nodelet owns Node registration.
# Starting flanneld before both exist only makes it exit once with the noisy
# "pod cidr not assigned" error before systemd retries it. Wait here instead:
# the service stays supervised, but the first flanneld process starts only
# when its lease can actually succeed. This is especially important because
# nodebootstrap deliberately starts nodecontroller before CNI to break the
# node-IPAM/flannel dependency cycle.
if [[ -n "${NODE_NAME:-}" ]]; then
    echo "==> waiting for nodecontroller to assign a PodCIDR to $NODE_NAME"
    pod_cidr=""
    for _ in {1..30}; do
        if pod_cidr="$(kubectl --kubeconfig "$KUBECONFIG" get node "$NODE_NAME" \
            -o jsonpath='{.spec.podCIDR}' 2>/dev/null)" && [[ -n "$pod_cidr" ]]; then
            break
        fi
        sleep 2
    done
    if [[ -z "$pod_cidr" ]]; then
        echo "error: timed out waiting for PodCIDR: PATH=$PATH NODE_NAME=$NODE_NAME; check apiserver access and nodecontroller" >&2
        exit 1
    fi
    echo "==> node PodCIDR ready: $pod_cidr"
fi

v4_net="" v6_net=""
[[ "$IP_FAMILY" == "dual" || "$IP_FAMILY" == "ipv4" ]] && v4_net="$IPV4_CLUSTER_CIDR"
[[ "$IP_FAMILY" == "dual" || "$IP_FAMILY" == "ipv6" ]] && v6_net="$IPV6_CLUSTER_CIDR"

cat > /etc/kube-flannel/net-conf.json <<EOF
{
  "Network": "${v4_net:-$IPV4_CLUSTER_CIDR}",
  "EnableIPv4": $( [[ -n "$v4_net" ]] && echo true || echo false ),
  "EnableIPv6": $( [[ -n "$v6_net" ]] && echo true || echo false ),
  "IPv6Network": "${v6_net:-::/0}",
  "Backend": { "Type": "vxlan" }
}
EOF

# ── Which interface flanneld should use ──────────────────────────────────────
#
# Without --iface, flanneld derives its interface from the default route, per
# address family. That derivation fails on a *multipath* (ECMP) default route,
# because such a route carries no `dev` of its own — the outgoing interface
# appears only on its nexthop lines:
#
#   default proto ra metric 1024 expires 1789sec mtu 1500 pref medium
#           nexthop via fe80::dea6:32ff:fe91:8003 dev eth0 weight 1
#           nexthop via fe80::fe99:47ff:fe21:1f40 dev eth0 weight 1
#
# That is the ordinary shape of an IPv6 default route on a LAN with two
# RA-advertising routers, and it makes flanneld exit at startup with
#
#   Failed to find any valid interface to use: failed to get default v6
#   interface: Found default v6 route but could not determine interface
#
# then crash-loop forever under Restart=always — while IPv6 on the host works
# perfectly. Found on a real dual-stack network, where it took down every
# pod on the node (no subnet.env, so no CNI, so everything stuck Pending).
#
# Note this is a *different* failure from the one common.sh's detect_ipv6()
# guards against ("Unable to find default v6 route" — no route at all). A
# route exists here; it just doesn't name a single interface. So the fix
# belongs here rather than in the family detection: naming the interface
# explicitly skips the derivation for both families at once, and keeps
# dual-stack working instead of quietly degrading the node to IPv4.
flannel_default_iface() {
    local fam dev
    # v4 first: its default route is almost always a simple single-nexthop
    # route that names its device directly. Both families ride the same NIC
    # in every layout this deployment supports, and flanneld resolves each
    # family's address from the interface it is given.
    for fam in -4 -6; do
        dev="$(ip "$fam" route show default 2>/dev/null \
                | awk '{ for (i = 1; i < NF; i++) if ($i == "dev") { print $(i + 1); exit } }')"
        [[ -n "$dev" ]] && { printf '%s' "$dev"; return 0; }
    done
    return 1
}

IFACE_ARGS=()
if [[ -n "${FLANNEL_IFACE:-}" ]]; then
    # Explicit operator override always wins — a host with several NICs may
    # well want the overlay somewhere other than the default route's device.
    IFACE_ARGS=(--iface="$FLANNEL_IFACE")
    echo "==> flanneld interface: $FLANNEL_IFACE (from FLANNEL_IFACE)"
elif iface="$(flannel_default_iface)"; then
    IFACE_ARGS=(--iface="$iface")
    echo "==> flanneld interface: $iface (from the default route)"
else
    # Not fatal: with no default route at all, flanneld's own derivation is
    # no worse off than ours, and its error message is the better one to
    # surface.
    echo "WARNING: no default route names an interface — letting flanneld derive one." >&2
fi

# exec, not just run: signals (SIGTERM from systemd) go straight to
# flanneld, and there's no wrapper-shell zombie left behind.
exec "$FLANNELD_BIN" --kube-subnet-mgr --ip-masq \
    --kubeconfig-file="$KUBECONFIG" \
    --net-config-path=/etc/kube-flannel/net-conf.json \
    "${IFACE_ARGS[@]}"
