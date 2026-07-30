#!/usr/bin/env bash
# diagnose-cni.sh — quick check for pods stuck Pending/ContainerCreating
# under --with-cri.
#
# Two things this looks for:
#   1. Is containerd's configured CNI bin_dir the same place the CNI plugin
#      binaries (bridge/flannel/host-local) actually got installed? A
#      mismatch there means RunPodSandbox can't find the plugins, and
#      nodelet just logs a warning rather than surfacing it anywhere obvious.
#   2. Is the pod actually reaching CNI/containerd at all, or is it stuck
#      earlier, unscheduled — e.g. a `node.kubernetes.io/network-unavailable`
#      taint, which Kubernetes applies until something (flannel has a
#      -set-node-network-unavailable flag for exactly this) clears the
#      node's NetworkUnavailable condition. If the pod's own status shows
#      "Unschedulable" with an untolerated taint, CNI/containerd are not
#      the problem yet — the scheduler never got that far.
#
# Usage:
#   sudo ./deploy/diagnose-cni.sh [node-name] [pod-name]
#   # defaults: node-name from `hostname`, pod-name smoke-test
set -uo pipefail

NODE="${1:-$(hostname)}"
POD="${2:-smoke-test}"

echo "=== containerd CNI config ==="
sudo grep -A2 'cni"\]' /etc/containerd/config.toml

echo "=== /opt/cni/bin ==="
ls -la /opt/cni/bin/ 2>&1

echo "=== /usr/lib/cni ==="
ls -la /usr/lib/cni/ 2>&1

echo "=== /usr/libexec/cni ==="
ls -la /usr/libexec/cni/ 2>&1

echo "=== node taints ==="
kubectl get node "$NODE" -o jsonpath='{.spec.taints}'
echo

echo "=== node conditions ==="
kubectl get node "$NODE" -o jsonpath='{.status.conditions}' | tr ',' '\n'
echo

echo "=== flannel subnet ==="
cat /run/flannel/subnet.env 2>&1

echo "=== flanneld recent journal ==="
sudo journalctl -u flanneld -n 30 --no-pager

echo "=== nodelet journal ==="
sudo journalctl -u nodelet -n 60 --no-pager

echo "=== pod status ($POD) ==="
kubectl get pod "$POD" -o yaml
