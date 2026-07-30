#!/usr/bin/env bash
# diagnose-coredns-scaleup.sh — captures everything needed to root-cause
# coredns pods multiplying past their desired replica count while nodelet
# is running. nodelet cannot create Pod API objects itself (only
# .get_opt()/.patch_status() — verified by grepping the whole crate), so if
# it's the trigger it has to be indirect: nodelet doing something that
# destabilizes the apiserver/controller-manager enough that k3s's own
# ReplicaSet controller starts over-creating. This captures both sides
# (nodelet's own journal + the k3s/RS side) plus a live sample loop so the
# two can be correlated by timestamp, since the whole point is figuring out
# which one moves first.
#
# Usage:
#   ./deploy/diagnose-coredns-scaleup.sh            # one-shot snapshot
#   ./deploy/diagnose-coredns-scaleup.sh --watch     # also samples pod count
#                                                     # + nodelet restarts every
#                                                     # 3s for 90s (run this
#                                                     # WHILE the scale-up is
#                                                     # actively happening)
#
# Writes everything to a single timestamped file under /tmp and prints its
# path — paste that file's contents back (or attach it).
set -uo pipefail

OUT="/tmp/notk8s-diagnose-coredns-$(date +%Y%m%d-%H%M%S).txt"
WATCH=0
[[ "${1:-}" == "--watch" ]] && WATCH=1

exec > >(tee "$OUT") 2>&1

section() { printf '\n========== %s ==========\n' "$1"; }

section "current pods (all namespaces)"
sudo k3s kubectl get pods -A -o wide

section "coredns Deployment desired replicas"
sudo k3s kubectl get deploy coredns -n kube-system -o jsonpath='{.spec.replicas}{"\n"}' 2>&1

section "coredns ReplicaSet(s)"
sudo k3s kubectl get rs -n kube-system -l k8s-app=kube-dns -o wide 2>&1

section "coredns-related events, oldest to newest"
sudo k3s kubectl get events -n kube-system --sort-by=.lastTimestamp 2>&1 | grep -i coredns

section "nodelet.service status"
sudo systemctl status nodelet --no-pager -l 2>&1

section "nodelet restart count + since-boot summary"
sudo systemctl show nodelet -p NRestarts -p ActiveEnterTimestamp -p ExecMainStartTimestamp 2>&1

section "nodelet's OWN journal, full detail, last 45 minutes"
sudo journalctl -u nodelet --since "45 minutes ago" --no-pager 2>&1

section "k3s's OWN journal, last 45 minutes, filtered to node/pod/coredns/replicaset lines"
sudo journalctl -u k3s --since "45 minutes ago" --no-pager 2>&1 | grep -iE "coredns|replicaset|node.?lifecycle|debian|panic|error" | tail -300

if [[ "$WATCH" -eq 1 ]]; then
    section "live sample: coredns pod count + nodelet restart count, every 3s for 90s"
    echo "timestamp,coredns_pod_count,nodelet_nrestarts,nodelet_active_state"
    for i in $(seq 1 30); do
        ts="$(date -Iseconds)"
        count="$(sudo k3s kubectl get pods -n kube-system -l k8s-app=kube-dns --no-headers 2>/dev/null | wc -l)"
        restarts="$(sudo systemctl show nodelet -p NRestarts --value 2>/dev/null)"
        state="$(sudo systemctl show nodelet -p ActiveState --value 2>/dev/null)"
        echo "$ts,$count,$restarts,$state"
        sleep 3
    done
fi

section "done"
echo "Full output saved to: $OUT"
echo "Paste this whole file back, or run: cat $OUT"
