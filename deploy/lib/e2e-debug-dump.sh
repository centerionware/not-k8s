#!/usr/bin/env bash
# e2e-debug-dump.sh — print everything needed to diagnose a stuck/failed
# e2e run: node status/taints/conditions, disk space, and nodelet's own
# service status + recent logs. Exists because the CSI/DRA reference
# drivers' own upstream deploy tooling only dumps *their* objects on
# timeout (see e2e-full-setup.sh) — when the real problem is upstream of
# that (the node never went Ready, a disk-pressure taint, nodelet crashed
# or hung reconciling) there was previously no visibility into it at all,
# just "waiting for hostpath deployment to complete" forever with no
# signal either way. Found live: round 123's first real e2e run in CI hit
# exactly this blind spot.
set -uo pipefail

KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"
export KUBECONFIG

echo "=========================================="
echo "e2e-debug-dump: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "=========================================="

echo ""
echo "── disk space ──"
df -h / 2>&1

echo ""
echo "── node status ──"
kubectl get nodes -o wide 2>&1
echo ""
kubectl describe nodes 2>&1

echo ""
echo "── all pods (every namespace) ──"
kubectl get pods -A -o wide 2>&1

echo ""
echo "── pods not Running/Completed ──"
kubectl get pods -A -o wide 2>&1 | awk 'NR==1 || ($4 !~ /Running|Completed/)'

echo ""
echo "── nodelet.service ──"
sudo systemctl status nodelet.service --no-pager -l 2>&1 || echo "(nodelet.service not found/not running)"
echo ""
echo "── nodelet.service logs (last 200 lines) ──"
sudo journalctl -u nodelet.service --no-pager -n 200 2>&1 || echo "(no journalctl access)"

echo ""
echo "── k3s.service ──"
sudo systemctl status k3s.service --no-pager -l 2>&1 || echo "(k3s.service not found/not running)"

echo ""
echo "── containerd.service ──"
sudo systemctl status containerd.service --no-pager -l 2>&1 || echo "(containerd.service not found/not running)"

echo "=========================================="
echo "e2e-debug-dump: end"
echo "=========================================="
