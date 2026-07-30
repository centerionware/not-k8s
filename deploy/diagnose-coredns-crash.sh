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

# kubectl logs proxies through the kubelet's own HTTP server on :10250 —
# nodelet doesn't implement that (it's not a full kubelet), so kubectl logs
# always 502s here regardless of what's actually happening to the
# container. Not today's bug — just means crictl (talking to containerd
# directly) is the only way to get real logs/exit info in this setup.
#
# Also: this box has TWO containerd sockets. k3s's own embedded one at
# /run/k3s/containerd/containerd.sock (idle — --disable-agent means k3s
# never starts it) is crictl's default, and the one nodelet actually uses
# (a separately installed containerd) is /run/containerd/containerd.sock.
# Must point crictl at the second one explicitly or every query below
# "connection refused"s against a containerd that was never running.
CRICTL="crictl --runtime-endpoint unix:///run/containerd/containerd.sock"

echo "=== crictl ps -a, unfiltered (crictl's --name filter came back empty even for a container the apiserver confirms is running — listing everything instead) ==="
sudo $CRICTL ps -a

# The apiserver's own record of the current container ID is authoritative —
# ask it directly instead of trusting crictl's own listing/filtering, which
# didn't find a container by name that definitely exists.
CID="$(sudo k3s kubectl get pod -n kube-system "$POD" -o jsonpath='{.status.containerStatuses[0].containerID}' 2>/dev/null | sed 's#^containerd://##')"
echo "=== container ID per the apiserver: ${CID:-<none>} ==="

if [[ -n "$CID" ]]; then
    echo "--- crictl inspect $CID (reason/exitCode/message/state) ---"
    sudo $CRICTL inspect "$CID" 2>&1 | grep -B1 -A5 '"reason"\|"exitCode"\|"message"\|"state"'
    echo "--- crictl logs $CID ---"
    sudo $CRICTL logs "$CID" 2>&1 | tail -100
else
    echo "apiserver has no containerID on record for this pod right now"
fi

echo "=== done ==="
echo "Full output saved to: $OUT"
