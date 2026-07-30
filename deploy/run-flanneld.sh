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
# Env vars (all provided by bootstrap-test.sh's systemd/OpenRC unit):
#   FLANNELD_BIN          absolute path to the flanneld binary (required —
#                         service managers give this script a minimal PATH,
#                         so `command -v flanneld` can't be trusted here)
#   KUBECONFIG            path to the kubeconfig flanneld's kube-subnet-mgr uses
#   IP_FAMILY             ipv4 | ipv6 | dual (default: ipv4)
#   IPV4_CLUSTER_CIDR     default: 10.42.0.0/16
#   IPV6_CLUSTER_CIDR     default: fd00:42::/56
set -euo pipefail

: "${FLANNELD_BIN:?FLANNELD_BIN must be set to an absolute path to the flanneld binary}"
: "${KUBECONFIG:?KUBECONFIG must be set}"
IP_FAMILY="${IP_FAMILY:-ipv4}"
IPV4_CLUSTER_CIDR="${IPV4_CLUSTER_CIDR:-10.42.0.0/16}"
IPV6_CLUSTER_CIDR="${IPV6_CLUSTER_CIDR:-fd00:42::/56}"

mkdir -p /etc/kube-flannel /run/flannel

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

# exec, not just run: signals (SIGTERM from systemd) go straight to
# flanneld, and there's no wrapper-shell zombie left behind.
exec "$FLANNELD_BIN" --kube-subnet-mgr --ip-masq \
    --kubeconfig-file="$KUBECONFIG" \
    --net-config-path=/etc/kube-flannel/net-conf.json
