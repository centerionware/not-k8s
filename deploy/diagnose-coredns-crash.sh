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

echo "=== crictl ps -a, unfiltered ==="
sudo $CRICTL ps -a

# crictl inspect/logs on the apiserver's on-record container ID kept
# "not found"ing — the container is apparently crashing and getting
# removed (nodelet's restart-on-exit logic, working as designed now) faster
# than a multi-step kubectl+crictl round trip can catch up with it. Read the
# log FILE directly instead: sandbox_config() in cri.rs sets a real
# kubelet-style log_directory (/var/log/pods/<ns>_<name>_<uid>/), and that
# file persists on disk regardless of whether the container that wrote it
# still exists in containerd. This sidesteps the race entirely.
UID_="$(sudo k3s kubectl get pod -n kube-system "$POD" -o jsonpath='{.metadata.uid}' 2>/dev/null)"
LOGDIR="/var/log/pods/kube-system_${POD}_${UID_}"
echo "=== log directory: $LOGDIR ==="
sudo ls -la "$LOGDIR" 2>&1
echo "=== coredns container log file (persists across restarts) ==="
sudo find "$LOGDIR" -name 'coredns*.log' -exec sh -c 'echo "--- {} ---"; tail -150 "{}"' \; 2>&1

echo "=== also checking for older pod UIDs under /var/log/pods (in case this pod has already been recreated since) ==="
sudo find /var/log/pods -maxdepth 1 -iname '*coredns*' 2>&1

echo "=== done ==="
echo "Full output saved to: $OUT"
