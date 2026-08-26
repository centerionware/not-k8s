#!/usr/bin/env bash
# e2e-full-setup.sh — install the two reference drivers the full e2e suite
# needs to exercise CSI and DRA/CDI for real, rather than skipping those
# tests: kubernetes-csi/csi-driver-host-path (the reference CSI driver
# real Kubernetes e2e/conformance tests use — round 117-120's own
# verification used this) and kubernetes-sigs/dra-example-driver (DRA's
# equivalent reference driver — round 121's verification used this).
#
# Round 124: hugepages reservation + grpcurl install moved OUT to their
# own e2e-misc-prereqs.sh — those aren't CSI/DRA-gated at all, and this
# script only runs on the (now not all) e2e shards that draw a
# csi_dra-tagged test (harness.sh's NUM_DRIVER_SHARDS); bundling
# unrelated prerequisites in here would have silently skipped the
# hugepages/PodResources tests on every shard that doesn't.
#
# Deliberately fetches each driver's real upstream deploy tooling instead
# of hand-reconstructing manifests: round 121 found a hand-reconstructed
# gRPC proto silently wrong on a real driver after living unverified for
# many rounds — vendoring from the authoritative source instead of
# transcribing by hand is the same lesson applied here to deployment
# manifests, not just code.
#
# The one compatibility adjustment below is applied only to the downloaded
# copy. Kubernetes 1.34's apiserver rejects the v6.3 sidecar's WatchList
# requests with a retryable 429 while the replacement control plane's watch
# caches are coming up. Normal LIST/WATCH is the supported fallback and keeps
# the reference driver's real deploy script, RBAC, images, and probes intact.
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

# Keep the upstream deployment authoritative while adapting the downloaded
# manifest to this control plane's API compatibility and kubectl version.
# This is intentionally a narrow text change in WORK_DIR, never a checked-in
# copy of the driver's YAML.
patch_hostpath_deploy_tooling() {
    local driver_dir="$WORK_DIR/csi-driver-host-path/deploy/kubernetes-latest"
    local plugin_yaml="$driver_dir/hostpath/csi-hostpath-plugin.yaml"
    local deploy_sh="$driver_dir/deploy.sh"

    [[ -f "$plugin_yaml" && -f "$deploy_sh" ]] || {
        echo "hostpath deploy layout changed; refusing an unverified compatibility patch" >&2
        return 1
    }

    # Topology is GA/default in external-provisioner v6.3. WatchListClient is
    # a client-go feature, not an external-provisioner feature-gate flag: the
    # latter makes v6.3 exit immediately with status 255. Configure client-go
    # through its supported environment variable so this apiserver/backend
    # pair uses the standard LIST followed by WATCH path.
    sed -i \
        '/^        - name: csi-provisioner$/a\
          env:\
            - name: KUBE_FEATURE_WatchListClient\
              value: "false"' \
        "$plugin_yaml"

    # kubectl's current kustomize rejects the upstream deploy script's
    # deprecated commonLabels spelling. Preserve the same selector-inclusive
    # labels using the current Kustomization form in the downloaded script.
    sed -i \
        -e 's/^commonLabels:$/labels:\n- pairs:/' \
        -e '/^  app\.kubernetes\.io\// s/^  /    /' \
        -e '/^    app\.kubernetes\.io\/part-of: csi-driver-host-path$/a\  includeSelectors: true' \
        "$deploy_sh"

    # Two containers expose a port named healthz. Kubernetes accepts the
    # manifest but warns because port names must be unique within a Pod.
    # Rename only the registrar's port and its matching probe.
    sed -i \
        -e '/- name: node-driver-registrar/,/- name: liveness-probe/ s/name: healthz/name: reg-healthz/' \
        -e '/- name: node-driver-registrar/,/- name: liveness-probe/ s/port: healthz/port: reg-healthz/' \
        "$plugin_yaml"
}

patch_hostpath_deploy_tooling

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
---
# WaitForFirstConsumer, deliberately distinct from the two above: nodescheduler's
# VolumeBinding plugin only exercises its delayed-binding path (allowedTopologies,
# CSIStorageCapacity, the PreBind selected-node annotation and poll) against a
# class in this mode — an Immediate-only harness would leave that whole path
# untested. See deploy/lib/test/cases/scheduler.sh's WaitForFirstConsumer case.
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: csi-hostpath-sc-wait
provisioner: hostpath.csi.k8s.io
reclaimPolicy: Delete
volumeBindingMode: WaitForFirstConsumer
EOF

# Do not merely apply the reference manifests and assume the CSI sidecars
# are usable. A PVC that never leaves Pending is exactly what release run 50
# reported, and the suite otherwise spends a minute per test rediscovering
# the same broken provisioning path. This deliberately exercises the real
# external-provisioner -> apiserver -> nodecontroller path before the tests
# start, including the replacement controller's shared PV/PVC watches.
wait_for_csi_provisioning() {
    local name="nodebootstrap-csi-readiness"
    kubectl delete pvc "$name" --ignore-not-found --wait=true --timeout=30s >/dev/null 2>&1 || true
    kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $name
  namespace: default
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Mi
  storageClassName: csi-hostpath-sc
EOF

    local phase
    for i in $(seq 1 60); do
        phase="$(kubectl get pvc "$name" -o jsonpath='{.status.phase}' 2>/dev/null || true)"
        if [[ "$phase" == "Bound" ]]; then
            log "CSI readiness PVC bound successfully"
            kubectl delete pvc "$name" --wait=false >/dev/null 2>&1 || true
            return 0
        fi
        sleep 2
    done
    kubectl describe pvc "$name" || true
    echo "=== CSI readiness objects ===" >&2
    kubectl get pvc,pv,storageclass -o wide 2>&1 || true
    echo "=== external-provisioner service-account permissions ===" >&2
    external_provisioner="system:serviceaccount:default:csi-hostpathplugin-sa"
    kubectl auth can-i get persistentvolumeclaims --as="$external_provisioner" 2>&1 || true
    kubectl auth can-i watch persistentvolumeclaims --as="$external_provisioner" 2>&1 || true
    kubectl auth can-i get persistentvolumes --all-namespaces --as="$external_provisioner" 2>&1 || true
    kubectl auth can-i create persistentvolumes --all-namespaces --as="$external_provisioner" 2>&1 || true
    kubectl auth can-i get storageclasses --all-namespaces --as="$external_provisioner" 2>&1 || true
    kubectl auth can-i get leases --namespace=default --as="$external_provisioner" 2>&1 || true
    prov_pod="$(kubectl get pods --all-namespaces --no-headers 2>/dev/null \
        | awk '$2 ~ /csi-hostpathplugin/ { print $1 "/" $2; exit }')"
    if [[ -n "$prov_pod" ]]; then
        prov_ns="${prov_pod%%/*}"
        prov_name="${prov_pod#*/}"
        echo "=== csi-provisioner pod describe ($prov_pod) ===" >&2
        kubectl describe pod "$prov_name" -n "$prov_ns" 2>&1 || true
        echo "=== csi-provisioner logs ($prov_pod) ===" >&2
        kubectl logs "$prov_name" -n "$prov_ns" -c csi-provisioner --tail=160 2>&1 || true
    fi
    kubectl delete pvc "$name" --wait=false >/dev/null 2>&1 || true
    echo "CSI readiness PVC never reached Bound; refusing to run CSI/DRA e2e tests" >&2
    return 1
}

wait_for_csi_provisioning

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
    # After the token-refresh delete below, the old DaemonSet pod remains
    # selectable while it is terminating. Waiting on that stale name can
    # consume the whole timeout even though the replacement is healthy.
    for i in $(seq 1 60); do
        pod="$(kubectl get pods -n dra-example-driver \
            -l app.kubernetes.io/component=kubeletplugin -o json 2>/dev/null \
            | jq -r '.items[] | select(.metadata.deletionTimestamp == null) | .metadata.name' \
            | head -n 1 || true)"
        if [[ -n "$pod" ]] \
            && kubectl wait --for=condition=ready "pod/$pod" \
                -n dra-example-driver --timeout=5s >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
    done
    kubectl get pods -n dra-example-driver -l app.kubernetes.io/component=kubeletplugin -o wide || true
    kubectl describe pods -n dra-example-driver -l app.kubernetes.io/component=kubeletplugin || true
    while IFS= read -r pod; do
        [[ -n "$pod" ]] || continue
        echo "=== DRA kubeletplugin logs ($pod) ===" >&2
        kubectl logs "$pod" -n dra-example-driver --all-containers=true \
            --timestamps --tail=200 2>&1 || true
        echo "=== DRA kubeletplugin previous logs ($pod) ===" >&2
        kubectl logs "$pod" -n dra-example-driver --all-containers=true \
            --timestamps --previous --tail=200 2>&1 || true
    done < <(kubectl get pods -n dra-example-driver \
        -l app.kubernetes.io/component=kubeletplugin -o name 2>/dev/null || true)
    echo "DRA kubeletplugin pod never became Ready" >&2
    return 1
}

wait_for_nodelet_dra_registration() {
    local since="$1" line
    for i in $(seq 1 60); do
        if command -v journalctl &>/dev/null; then
            if journalctl -u nodelet --since "$since" --no-pager -o cat 2>/dev/null \
                | sed $'s/\\033\\[[0-9;]*m//g' \
                | grep -q 'plugin registered.*gpu.example.com'; then
                log "nodelet confirmed DRA plugin registration"
                return 0
            fi
        elif [[ -f /var/log/nodelet.log ]] \
            && sed $'s/\\033\\[[0-9;]*m//g' /var/log/nodelet.log \
                | grep -q 'plugin registered.*gpu.example.com'; then
            log "nodelet confirmed DRA plugin registration"
            return 0
        fi
        sleep 2
    done
    if command -v journalctl &>/dev/null; then
        line="$(journalctl -u nodelet --since "$since" --no-pager -o cat 2>&1 \
            | sed $'s/\\033\\[[0-9;]*m//g' | tail -80 || true)"
    else
        line="$(sed $'s/\\033\\[[0-9;]*m//g' /var/log/nodelet.log 2>&1 \
            | tail -80 || true)"
    fi
    printf '%s\n' "$line" >&2
    echo "DRA ResourceSlice appeared, but nodelet never confirmed plugin registration" >&2
    return 1
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
# Record the boundary before replacing the pod so a stale registration line
# from a previous partial setup cannot satisfy the readiness check below.
DRA_REGISTRATION_SINCE="$(date -u '+%Y-%m-%d %H:%M:%S')"
kubectl delete pod -n dra-example-driver -l app.kubernetes.io/component=kubeletplugin --ignore-not-found
# The old registrar socket is not removed synchronously with pod deletion.
# Leaving it behind makes nodelet retry a dead endpoint forever, which was the
# recurring warning in release run 50 and also masked the new driver's
# registration. The driver owns these sockets, so remove only its stale
# registration endpoints before the replacement pod starts.
find "$NODELET_DATA_DIR/plugins_registry" -maxdepth 1 -type s \
    -name 'gpu.example.com-*-reg.sock' -delete 2>/dev/null || true
wait_for_dra_pod_ready

log "confirming both drivers actually registered with nodelet..."
drivers_registered=false
for i in $(seq 1 15); do
    if kubectl get csinodes -o jsonpath='{.items[0].spec.drivers[*].name}' 2>/dev/null | grep -q hostpath.csi.k8s.io \
        && kubectl get resourceslices -o name 2>/dev/null | grep -q resourceslice; then
        drivers_registered=true
        break
    fi
    sleep 4
done
[[ "$drivers_registered" == true ]] || {
    kubectl get csinodes -o yaml || true
    kubectl get resourceslices -o yaml || true
    echo "reference CSI/DRA resources never appeared in the apiserver" >&2
    exit 1
}
wait_for_nodelet_dra_registration "$DRA_REGISTRATION_SINCE"

# ── env vars the e2e suite's CSI/DRA-gated tests key off ───────────────────
ENV_FILE="${GITHUB_ENV:-$WORK_DIR/e2e-setup.env}"
{
    # dra-example-driver's own DeviceClass (see its demo/examples/*/*.yaml —
    # every one of them references "gpu.example.com"), matching the
    # numDevices=4 fake-GPU pool the helm install above configured. Round
    # 123: previously nothing wrote this, so dra.sh's real allocation test
    # stayed manual-only even though the driver was already installed here.
    echo "TEST_DRA_DEVICE_CLASS=gpu.example.com"
    echo "TEST_CSI_STORAGE_CLASS=csi-hostpath-sc"
    echo "TEST_CSI_ATTACH_STORAGE_CLASS=csi-hostpath-sc"
    echo "TEST_CSI_INLINE_DRIVER=hostpath.csi.k8s.io"
    echo "TEST_CSI_BLOCK_STORAGE_CLASS=csi-hostpath-block-sc"
    # WaitForFirstConsumer — nodescheduler's VolumeBinding.sh case needs a
    # class in this mode to exercise its delayed-binding path at all; the
    # three above are all Immediate.
    echo "TEST_CSI_STORAGE_CLASS_WAIT=csi-hostpath-sc-wait"
} >> "$ENV_FILE"
log "wrote e2e env vars to $ENV_FILE"

[[ -z "${E2E_SETUP_WORK_DIR:-}" ]] && rm -rf "$WORK_DIR"
log "done."
