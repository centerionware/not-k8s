#!/usr/bin/env bash
# diagnose-coredns-crash.sh — why is coredns's container actually exiting?
# The pile-up mechanism (nodelet never restarting a crashed container, and
# mislabeling the pod terminal) is fixed; this is the separate question of
# what's crashing it in the first place.
set -uo pipefail

OUT="/tmp/notk8s-diagnose-crash-$(date +%Y%m%d-%H%M%S).txt"
exec > >(tee "$OUT") 2>&1

POD="$(sudo k3s kubectl get pods -n kube-system -l k8s-app=kube-dns -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)"
echo "=== pod: $POD ==="

echo "=== kubectl describe (events, last state / exit code / reason) ==="
sudo k3s kubectl describe pod -n kube-system "$POD"

echo "=== current container logs ==="
sudo k3s kubectl logs -n kube-system "$POD" 2>&1

echo "=== previous container logs (if a prior instance's logs are still available) ==="
sudo k3s kubectl logs -n kube-system "$POD" --previous 2>&1

echo "=== crictl ps -a for coredns (real exit codes, if crictl is available) ==="
if command -v crictl &>/dev/null; then
    sudo crictl ps -a --name coredns
    id="$(sudo crictl ps -a --name coredns -q | head -1)"
    [[ -n "$id" ]] && sudo crictl inspect "$id" | grep -A5 '"reason"\|"exitCode"\|"message"'
else
    echo "crictl not on PATH"
fi

echo "=== done ==="
echo "Full output saved to: $OUT"
