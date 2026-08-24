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

kubectl_cmd() {
    kubectl --kubeconfig "$KUBECONFIG" "$@"
}

echo "=========================================="
echo "e2e-debug-dump: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "=========================================="

echo ""
echo "── disk space ──"
df -h / 2>&1

echo ""
echo "── node status ──"
kubectl_cmd get nodes -o wide 2>&1
echo ""
kubectl_cmd describe nodes 2>&1

echo ""
echo "── all pods (every namespace) ──"
kubectl_cmd get pods -A -o wide 2>&1

echo ""
echo "── pods not Running/Completed ──"
kubectl_cmd get pods -A -o wide 2>&1 | awk 'NR==1 || ($4 !~ /Running|Completed/)'

echo ""
echo "── nodelet.service ──"
sudo systemctl status nodelet.service --no-pager -l 2>&1 || echo "(nodelet.service not found/not running)"
echo ""
echo "── nodelet.service logs (last 200 lines) ──"
sudo journalctl -u nodelet.service --no-pager -n 200 2>&1 || echo "(no journalctl access)"

echo ""
# Only present when this deployment runs our scheduler (SCHEDULER=nodescheduler).
# Worth dumping unconditionally rather than gating on it: when placement is
# what broke, "the unit is not installed" is itself the answer, and the first
# live run of nodescheduler was diagnosed almost blind for want of exactly
# this — the dump tailed nodelet and said nothing about what decided the
# placement.
echo "── nodescheduler.service ──"
sudo systemctl status nodescheduler.service --no-pager -l 2>&1 || echo "(nodescheduler.service not found/not running — if SCHEDULER=nodescheduler this deployment passed --disable-scheduler to k3s too, so pods are placed by neither and stay Pending, not by k3s's own scheduler)"
echo ""
echo "── nodescheduler.service logs (last 200 lines) ──"
sudo journalctl -u nodescheduler.service --no-pager -n 200 2>&1 || echo "(no journalctl access)"
echo ""
echo "── scheduler lease ──"
kubectl_cmd get lease kube-scheduler -n kube-system -o yaml 2>&1 | head -30 || echo "(no scheduler lease)"

echo ""
# Same reasoning as the nodescheduler dump above — worth it unconditionally.
echo "── nodecontroller.service ──"
sudo systemctl status nodecontroller.service --no-pager -l 2>&1 || echo "(nodecontroller.service not found/not running — if CONTROLLER_MANAGER=nodecontroller this deployment passed --disable-controller-manager to k3s too, so node lifecycle/podCIDR/GC are done by neither)"
echo ""
echo "── nodecontroller.service logs (last 200 lines) ──"
sudo journalctl -u nodecontroller.service --no-pager -n 200 2>&1 || echo "(no journalctl access)"
echo ""
echo "── controller-manager lease ──"
kubectl_cmd get lease kube-controller-manager -n kube-system -o yaml 2>&1 | head -30 || echo "(no controller-manager lease)"

echo ""
echo "── k3s.service ──"
sudo systemctl status k3s.service --no-pager -l 2>&1 || echo "(k3s.service not found/not running)"

echo ""
# k3s bundles kube-controller-manager (AttachDetachController et al) into
# the same process/journal as everything else it stripped down to — there
# is no separate unit to target. A stuck CSI attach (VolumeAttachment never
# created, or created but never Attached) is invisible without this: the
# nodelet/nodescheduler dumps above only show the two ends of that pipe,
# never the controller in the middle. Grepped rather than dumped whole
# because k3s's own log is dominated by apiserver audit/watch chatter —
# unfiltered this would bury the one subsystem actually worth reading here
# under everything else running in the same process.
echo "── k3s.service logs: attach/detach + volume events (last 800 lines, filtered) ──"
sudo journalctl -u k3s.service --no-pager -n 800 2>&1 \
    | grep -iE "attachdetach|volumeattachment|persistentvolume|reconciler|csidriver|csinode|nodeipam|cidr|desiredstateofworld|actualstateofworld|populat" \
    || echo "(no matching lines in the last 800 — either nothing ran, or it's further back than this tail reaches)"

echo ""
echo "── volumeattachments (every namespace is cluster-scoped) ──"
kubectl_cmd get volumeattachments.storage.k8s.io -o wide 2>&1 || echo "(none / apiserver unreachable)"

echo ""
# CSIDriver/CSINode survive a failing test's own cleanup (it only deletes
# its Pod/PVC), so unlike the pod-scoped events below these still reflect
# real state at dump time — worth having verbatim rather than inferred
# from nodelet's registration-time logs, which only prove registration
# happened once, not that it's still correct now.
echo "── csidrivers ──"
kubectl_cmd get csidrivers.storage.k8s.io -o yaml 2>&1 || echo "(none / apiserver unreachable)"

echo ""
echo "── csinodes ──"
kubectl_cmd get csinodes.storage.k8s.io -o yaml 2>&1 || echo "(none / apiserver unreachable)"

echo ""
echo "── recent events, every namespace (last 100, by time) ──"
kubectl_cmd get events -A --sort-by=.lastTimestamp 2>&1 | tail -100

echo ""
echo "── containerd.service ──"
sudo systemctl status containerd.service --no-pager -l 2>&1 || echo "(containerd.service not found/not running)"

# Ground truth for nodecontroller's impersonated-SA writes (crates/
# nodebootstrap/src/rbac.rs's Finding #4): confirms whether the real
# system:controller:<name> ClusterRole this stack's k3s-embedded apiserver
# is supposed to seed actually exists, and whether the impersonated SA
# identity can really use it -- rather than inferring it from a 403
# message alone.
echo ""
echo "── system:controller:replicaset-controller ClusterRole (ground truth) ──"
kubectl_cmd get clusterrole system:controller:replicaset-controller -o yaml 2>&1 || echo "(missing)"

echo ""
echo "── clusterrolebindings naming replicaset-controller (real + supplemental) ──"
kubectl_cmd get clusterrolebindings -o yaml 2>&1 | grep -B5 -A15 'name: replicaset-controller\|controller-sa-replicaset-controller' \
    || echo "(none found)"

echo ""
echo "── kubectl auth can-i, as the impersonated identity itself ──"
kubectl_cmd auth can-i patch replicasets/status --as=system:serviceaccount:kube-system:replicaset-controller -n kube-system 2>&1
kubectl_cmd auth can-i update replicasets/status --as=system:serviceaccount:kube-system:replicaset-controller -n kube-system 2>&1
kubectl_cmd auth can-i patch endpointslices --as=system:serviceaccount:kube-system:endpointslice-controller -n kube-system 2>&1
kubectl_cmd auth can-i update endpointslices --as=system:serviceaccount:kube-system:endpointslice-controller -n kube-system 2>&1

echo ""
echo "── system:controller:endpointslice-controller ClusterRole (ground truth) ──"
kubectl_cmd get clusterrole system:controller:endpointslice-controller -o yaml 2>&1 || echo "(missing)"

echo "=========================================="
echo "e2e-debug-dump: end"
echo "=========================================="
