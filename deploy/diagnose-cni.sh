#!/usr/bin/env bash
# diagnose-cni.sh — quick check for pods stuck Pending/ContainerCreating
# under --with-cri: is containerd's configured CNI bin_dir the same place
# the CNI plugin binaries (bridge/flannel/host-local) actually got
# installed? A mismatch there means RunPodSandbox can't find the plugins,
# and nodelet just logs a warning rather than surfacing it anywhere obvious.
#
# Usage:
#   sudo ./deploy/diagnose-cni.sh [pod-name]   # default pod-name: smoke-test
set -uo pipefail

POD="${1:-smoke-test}"

echo "=== containerd CNI config ==="
sudo grep -A2 'cni"\]' /etc/containerd/config.toml

echo "=== /opt/cni/bin ==="
ls -la /opt/cni/bin/ 2>&1

echo "=== /usr/lib/cni ==="
ls -la /usr/lib/cni/ 2>&1

echo "=== /usr/libexec/cni ==="
ls -la /usr/libexec/cni/ 2>&1

echo "=== nodelet journal ==="
sudo journalctl -u nodelet -n 60 --no-pager

echo "=== pod status ($POD) ==="
kubectl get pod "$POD" -o yaml
