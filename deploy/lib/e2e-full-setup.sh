#!/usr/bin/env bash
# e2e-full-setup.sh — install the two reference drivers the full e2e suite
# needs to exercise CSI and DRA/CDI for real, rather than skipping those
# tests: kubernetes-csi/csi-driver-host-path (the reference CSI driver
# real Kubernetes e2e/conformance tests use — round 117-120's own
# verification used this) and kubernetes-sigs/dra-example-driver (DRA's
# equivalent reference driver — round 121's verification used this).
#
# Deliberately fetches each driver's real upstream deploy tooling instead
# of hand-reconstructing manifests: round 121 found a hand-reconstructed
# gRPC proto silently wrong on a real driver after living unverified for
# many rounds — vendoring from the authoritative source instead of
# transcribing by hand is the same lesson applied here to deployment
# manifests, not just code.
#
# Assumes: a running not-k8s cluster (deploy/bootstrap-source.sh --with-cri
# already ran), kubectl configured via $KUBECONFIG, and network access to
# github.com/registry.k8s.io. Idempotent — safe to re-run against an
# already-set-up cluster (helm upgrade -i, kubectl apply).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
WORK_DIR="${E2E_SETUP_WORK_DIR:-$(mktemp -d)}"
NODELET_DATA_DIR="${NODELET_DATA_DIR:-/var/lib/nodelet}"

log() { echo "==> $*"; }

# ── helm ─────────────────────────────────────────────────────────────────
if ! command -v helm &>/dev/null; then
    log "installing helm..."
    HELM_VERSION="v3.16.4"
    ARCH_RAW="$(uname -m)"
    case "$ARCH_RAW" in
        x86_64) HELM_ARCH=amd64 ;;
        aarch64) HELM_ARCH=arm64 ;;
        armv7l) HELM_ARCH=arm ;;
        *) echo "unsupported arch for helm install: $ARCH_RAW" >&2; exit 1 ;;
    esac
    curl -fsSL "https://get.helm.sh/helm-${HELM_VERSION}-linux-${HELM_ARCH}.tar.gz" -o "$WORK_DIR/helm.tar.gz"
    tar -xzf "$WORK_DIR/helm.tar.gz" -C "$WORK_DIR"
    sudo install -m 0755 "$WORK_DIR/linux-${HELM_ARCH}/helm" /usr/local/bin/helm
fi

# The reference driver's own deploy.sh (below) unconditionally applies a
# VolumeSnapshotClass and hard-exits (its own set -e) if the
# snapshot.storage.k8s.io CRDs aren't already installed — this project
# doesn't test volume snapshots at all, but the CRDs cost nothing to
# install and the alternative is patching upstream's own deploy tooling,
# which is exactly the kind of drift-prone hand-editing this script exists
# to avoid.
log "installing VolumeSnapshot CRDs (needed by the reference driver's own deploy.sh)..."
kubectl apply -f https://raw.githubusercontent.com/kubernetes-csi/external-snapshotter/v8.6.0/client/config/crd/snapshot.storage.k8s.io_volumesnapshotclasses.yaml
kubectl apply -f https://raw.githubusercontent.com/kubernetes-csi/external-snapshotter/v8.6.0/client/config/crd/snapshot.storage.k8s.io_volumesnapshotcontents.yaml
kubectl apply -f https://raw.githubusercontent.com/kubernetes-csi/external-snapshotter/v8.6.0/client/config/crd/snapshot.storage.k8s.io_volumesnapshots.yaml

# ── CSI: kubernetes-csi/csi-driver-host-path's own real deploy.sh ──────────
log "fetching csi-driver-host-path..."
git clone --depth 1 https://github.com/kubernetes-csi/csi-driver-host-path.git "$WORK_DIR/csi-driver-host-path"

log "deploying hostpath CSI driver (KUBELET_DATA_DIR=$NODELET_DATA_DIR)..."
KUBELET_DATA_DIR="$NODELET_DATA_DIR" "$WORK_DIR/csi-driver-host-path/deploy/kubernetes-latest/deploy.sh"

log "applying StorageClasses for the e2e suite..."
kubectl apply -f - <<EOF
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: csi-hostpath-sc
provisioner: hostpath.csi.k8s.io
reclaimPolicy: Delete
volumeBindingMode: Immediate
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: csi-hostpath-block-sc
provisioner: hostpath.csi.k8s.io
reclaimPolicy: Delete
volumeBindingMode: Immediate
EOF

# ── DRA: kubernetes-sigs/dra-example-driver's own Helm chart ───────────────
log "fetching dra-example-driver..."
git clone --depth 1 https://github.com/kubernetes-sigs/dra-example-driver.git "$WORK_DIR/dra-example-driver"

log "installing dra-example-driver (kubeletPlugin paths -> $NODELET_DATA_DIR)..."
helm upgrade -i --create-namespace --namespace dra-example-driver \
    dra-example-driver "$WORK_DIR/dra-example-driver/deployments/helm/dra-example-driver" \
    --set kubeletPlugin.kubeletRegistrarDirectoryPath="$NODELET_DATA_DIR/plugins_registry" \
    --set kubeletPlugin.kubeletPluginsDirectoryPath="$NODELET_DATA_DIR/plugins" \
    --set kubeletPlugin.numDevices=4

# `kubectl wait -l <selector>` errors immediately with "no matching
# resources found" if zero pods match *at the moment it's called* — it
# does not wait for one to be *created*, only for an already-existing one
# to reach the condition. A DaemonSet's pod object doesn't exist the
# instant `helm install`/`kubectl delete` returns (real controller
# propagation delay), so a bare `kubectl wait` right after either
# routinely races and fails outright — confirmed for real, round 123's
# CI hit this exactly. Poll for the pod to *exist* first, then wait for
# it to become ready.
wait_for_dra_pod_ready() {
    for i in $(seq 1 30); do
        [[ -n "$(kubectl get pods -n dra-example-driver -l app.kubernetes.io/component=kubeletplugin -o name 2>/dev/null)" ]] && break
        sleep 2
    done
    kubectl wait --for=condition=ready pod -l app.kubernetes.io/component=kubeletplugin -n dra-example-driver --timeout=120s
}

log "waiting for the DRA driver pod to be ready..."
wait_for_dra_pod_ready

# A driver's own gRPC client TokenRequest needs a fresh, node-bound
# ServiceAccount token to satisfy the apiserver's ResourceSlice admission
# policy (round 121, bug 1) — the pod's very first token was minted before
# nodelet's bound_object_ref fix could possibly have mattered on a
# from-scratch cluster, but on a from-scratch cluster it's already correct
# from the start, so this is just making sure a stale pod from a prior
# partial run doesn't linger with an old token.
kubectl delete pod -n dra-example-driver -l app.kubernetes.io/component=kubeletplugin --ignore-not-found
wait_for_dra_pod_ready

log "confirming both drivers actually registered with nodelet..."
for i in $(seq 1 15); do
    kubectl get csinodes -o jsonpath='{.items[0].spec.drivers[*].name}' 2>/dev/null | grep -q hostpath.csi.k8s.io && \
        kubectl get resourceslices -o name 2>/dev/null | grep -q resourceslice && break
    sleep 4
done

# ── env vars the e2e suite's CSI/DRA-gated tests key off ───────────────────
ENV_FILE="${GITHUB_ENV:-$WORK_DIR/e2e-setup.env}"
{
    echo "TEST_CSI_STORAGE_CLASS=csi-hostpath-sc"
    echo "TEST_CSI_ATTACH_STORAGE_CLASS=csi-hostpath-sc"
    echo "TEST_CSI_INLINE_DRIVER=hostpath.csi.k8s.io"
    echo "TEST_CSI_BLOCK_STORAGE_CLASS=csi-hostpath-block-sc"
} >> "$ENV_FILE"
log "wrote e2e env vars to $ENV_FILE"

[[ -z "${E2E_SETUP_WORK_DIR:-}" ]] && rm -rf "$WORK_DIR"
log "done."
