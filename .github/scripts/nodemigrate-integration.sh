#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="${GITHUB_WORKSPACE:-$(git rev-parse --show-toplevel)}"
BIN_DIR="$ROOT/target/release"
NK="$BIN_DIR/notk8s"
MIGRATE="$BIN_DIR/nodemigrate"
LIBRARY_MODE="${NODEMIGRATE_INTEGRATION_LIBRARY:-false}"
if [[ "$LIBRARY_MODE" == true ]]; then
    SOURCE_DIST="${NODEMIGRATE_SOURCE_DIST:-kubernetes}"
else
    SOURCE_DIST="${NODEMIGRATE_SOURCE_DIST:?set NODEMIGRATE_SOURCE_DIST to k3s or kubernetes}"
fi
LOG="${NODEMIGRATE_TEST_LOG:-/tmp/nodemigrate-integration.log}"
WORK="${NODEMIGRATE_WORK_DIR:-/var/lib/nodemigrate-ci}"
STATIC_PATH="$WORK/static-volume"
CHECKPOINT_DIR="$WORK/checkpoints"
SOURCE_KUBECONFIG=""
CURRENT_KUBECONFIG=""

if [[ "$LIBRARY_MODE" != true ]]; then
    exec > >(tee -a "$LOG") 2>&1
fi

diagnostics() {
    status=$?
    if [[ $status -ne 0 ]]; then
        echo "Migration integration failed at $(date -u +%FT%TZ), exit=$status"
        echo "source=$SOURCE_DIST kubeconfig=${CURRENT_KUBECONFIG:-unset}"
        if [[ -n "$CURRENT_KUBECONFIG" && -f "$CURRENT_KUBECONFIG" ]]; then
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get nodes -o wide || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get pods,pvc,pv -A -o wide || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get events -A --sort-by=.lastTimestamp | tail -n 100 || true
        fi
        journalctl -b -u k3s -u kubelet -u containerd -u nodestore -u nodeapiserver \
            -u kube-apiserver --no-pager -n 250 || true
    fi
    exit "$status"
}
if [[ "$LIBRARY_MODE" != true ]]; then
    trap diagnostics EXIT
fi

need_root() {
    [[ "$(id -u)" == 0 ]] || { echo "run this integration script as root" >&2; exit 2; }
    [[ -x "$NK" && -x "$MIGRATE" ]] || {
        echo "build target/release/notk8s and target/release/nodemigrate first" >&2
        exit 2
    }
    [[ "$SOURCE_DIST" == k3s || "$SOURCE_DIST" == kubernetes ]] || {
        echo "unsupported source distribution: $SOURCE_DIST" >&2
        exit 2
    }
}

install_tools() {
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq apt-transport-https ca-certificates conntrack curl ebtables ethtool \
        gpg jq socat
    mkdir -p /etc/apt/keyrings
    local stable minor
    stable="$(curl -fsSL https://dl.k8s.io/release/stable.txt)"
    minor="$(sed -E 's/^(v[0-9]+\.[0-9]+)\..*/\1/' <<<"$stable")"
    curl -fsSL "https://pkgs.k8s.io/core:/stable:/${minor}/deb/Release.key" \
        | gpg --dearmor --yes -o /etc/apt/keyrings/kubernetes-apt-keyring.gpg
    printf 'deb [signed-by=/etc/apt/keyrings/kubernetes-apt-keyring.gpg] https://pkgs.k8s.io/core:/stable:/%s/deb/ /\n' \
        "$minor" > /etc/apt/sources.list.d/kubernetes.list
    apt-get update -qq
    apt-get install -y -qq kubectl

    local helm_version=v3.17.3
    if ! command -v helm >/dev/null 2>&1; then
        curl -fsSL "https://get.helm.sh/helm-${helm_version}-linux-amd64.tar.gz" -o /tmp/helm.tgz
        tar -xzf /tmp/helm.tgz -C /tmp
        install -m0755 /tmp/linux-amd64/helm /usr/local/bin/helm
    fi
    helm version --short
    kubectl version --client
}

install_containerd() {
    export NOTK8S_COMBINED_PREBUILT="$NK"
    export NODEBOOTSTRAP_COMBINED_SELF="$NK"
    export NOTK8S_BUILD_LAYOUT=combined
    export NODEBOOTSTRAP_REPO_ROOT="$ROOT"
    "$NK" bootstrap containerd
    systemctl is-active --quiet containerd
    ctr version
}

install_source() {
    install_containerd
    if [[ "$SOURCE_DIST" == k3s ]]; then
        local k3s_version="${K3S_VERSION:-v1.35.0+k3s1}"
        curl -sfL https://get.k3s.io -o /tmp/install-k3s.sh
        INSTALL_K3S_VERSION="$k3s_version" \
        INSTALL_K3S_EXEC='server --flannel-backend=none --disable-network-policy --disable=traefik --cluster-cidr=10.42.0.0/16 --write-kubeconfig-mode=644' \
            sh /tmp/install-k3s.sh
        SOURCE_KUBECONFIG=/etc/rancher/k3s/k3s.yaml
    else
        local stable minor
        stable="$(curl -fsSL https://dl.k8s.io/release/stable.txt)"
        minor="$(sed -E 's/^(v[0-9]+\.[0-9]+)\..*/\1/' <<<"$stable")"
        apt-get install -y -qq kubelet kubeadm
        local cri_tools_version="${CRI_TOOLS_VERSION:-${minor}.0}"
        local cri_tools_arch
        case "$(uname -m)" in
            x86_64) cri_tools_arch=amd64 ;;
            aarch64|arm64) cri_tools_arch=arm64 ;;
            *) echo "unsupported crictl architecture: $(uname -m)" >&2; return 1 ;;
        esac
        curl -fsSL "https://github.com/kubernetes-sigs/cri-tools/releases/download/${cri_tools_version}/crictl-${cri_tools_version}-linux-${cri_tools_arch}.tar.gz" \
            -o /tmp/crictl.tgz
        tar -xzf /tmp/crictl.tgz -C /usr/local/bin crictl
        chmod 0755 /usr/local/bin/crictl
        echo "cri-tools version=$cri_tools_version architecture=$cri_tools_arch"
        crictl --version
        apt-mark hold kubelet kubeadm kubectl
        swapoff -a
        sed -i.bak '/\sswap\s/s/^/#/' /etc/fstab
        kubeadm init \
            --kubernetes-version "$stable" \
            --pod-network-cidr=10.42.0.0/16 \
            --service-cidr=10.96.0.0/12 \
            --cri-socket=unix:///run/containerd/containerd.sock
        SOURCE_KUBECONFIG=/etc/kubernetes/admin.conf
        export KUBECONFIG="$SOURCE_KUBECONFIG"
        kubectl taint nodes --all node-role.kubernetes.io/control-plane- || true
        echo "upstream kubernetes version=$stable channel=$minor"
    fi
    export KUBECONFIG="$SOURCE_KUBECONFIG"
    CURRENT_KUBECONFIG="$SOURCE_KUBECONFIG"
    kubectl wait --for=condition=Ready node --all --timeout=180s || true
    kubectl get nodes -o wide
}

install_cilium() {
    local version="${CILIUM_VERSION:-1.20.2}"
    local cni_conf_path=/etc/cni/net.d
    local cni_bin_path=/opt/cni/bin
    if [[ "$SOURCE_DIST" == k3s ]]; then
        cni_conf_path=/var/lib/rancher/k3s/agent/etc/cni/net.d
        cni_bin_path=/var/lib/rancher/k3s/data/current/bin
    fi
    helm repo add cilium https://helm.cilium.io/ --force-update
    helm repo update cilium
    helm upgrade --install cilium cilium/cilium \
        --version "$version" --namespace kube-system \
        --set ipam.mode=kubernetes \
        --set cni.confPath="$cni_conf_path" \
        --set cni.binPath="$cni_bin_path" \
        --set kubeProxyReplacement=false \
        --set operator.replicas=1 \
        --set k8sServiceHost="${NODEMIGRATE_CILIUM_API_HOST:-127.0.0.1}" \
        --set k8sServicePort=6443 \
        --wait --timeout 10m
    kubectl rollout status daemonset/cilium -n kube-system --timeout=10m
    kubectl rollout status deployment/cilium-operator -n kube-system --timeout=10m
    kubectl wait --for=condition=Ready node --all --timeout=5m
}

install_hostpath_driver() {
    git -C "$ROOT" fetch --no-tags --depth=1 origin archive-shell-scripts-0.7.1
    git -C "$ROOT" show FETCH_HEAD:deploy/lib/e2e-full-setup.sh > /tmp/nodemigrate-hostpath-setup.sh
    timeout 600 bash /tmp/nodemigrate-hostpath-setup.sh
    kubectl get storageclass csi-hostpath-sc
}

install_workloads() {
    kubectl label nodes --all operator.example/pool=blue --overwrite
    kubectl annotate nodes --all nodemigrate.io/source-uid=operator-node-value --overwrite
    kubectl taint nodes --all operator.example/dedicated=migration:PreferNoSchedule --overwrite
    helm repo add jetstack https://charts.jetstack.io --force-update
    helm repo add traefik https://traefik.github.io/charts --force-update
    helm repo update
    helm upgrade --install cert-manager jetstack/cert-manager \
        --version "${CERT_MANAGER_VERSION:-v1.21.2}" \
        --namespace cert-manager --create-namespace --set crds.enabled=true \
        --wait --timeout 10m
    helm upgrade --install traefik traefik/traefik \
        --namespace traefik --create-namespace \
        --set service.type=ClusterIP --set ingressClass.enabled=true \
        --wait --timeout 10m

    mkdir -p "$STATIC_PATH"
    if [[ -n "${NODEMIGRATE_STATIC_NODE:-}" ]]; then
        [[ "$NODEMIGRATE_STATIC_NODE" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || {
            echo "invalid static PV node name: $NODEMIGRATE_STATIC_NODE" >&2
            return 1
        }
        kubectl apply -f - <<YAML
apiVersion: v1
kind: PersistentVolume
metadata:
  name: migration-static-pv
spec:
  capacity:
    storage: 1Gi
  accessModes: [ReadWriteOnce]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: migration-manual
  hostPath:
    path: /var/lib/nodemigrate-ci/static-volume
    type: Directory
  nodeAffinity:
    required:
      nodeSelectorTerms:
      - matchExpressions:
        - key: kubernetes.io/hostname
          operator: In
          values: ["$NODEMIGRATE_STATIC_NODE"]
YAML
    else
        kubectl apply -f - <<'YAML'
apiVersion: v1
kind: PersistentVolume
metadata:
  name: migration-static-pv
spec:
  capacity:
    storage: 1Gi
  accessModes: [ReadWriteOnce]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: migration-manual
  hostPath:
    path: /var/lib/nodemigrate-ci/static-volume
    type: Directory
YAML
    fi
    kubectl apply -f - <<'YAML'
apiVersion: v1
kind: Namespace
metadata:
  name: migration-apps
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: migration-user-metadata
  namespace: migration-apps
  annotations:
    nodemigrate.io/source-uid: user-value
data:
  migration-marker: user-config-data
---
apiVersion: v1
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: migration-static-pvc
  namespace: migration-apps
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: migration-manual
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: migration-csi-pvc
  namespace: migration-apps
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: csi-hostpath-sc
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: Pod
metadata:
  name: migration-seed
  namespace: migration-apps
spec:
  restartPolicy: Never
  initContainers:
  - name: seed-volumes
    image: busybox:1.36.1
    command: [sh, -c, 'echo static-persistent-data > /static/marker; echo csi-persistent-data > /csi/marker']
    volumeMounts:
    - name: static
      mountPath: /static
    - name: csi
      mountPath: /csi
  containers:
  - name: hold
    image: busybox:1.36.1
    command: [sh, -c, 'sleep 36000']
    volumeMounts:
    - name: static
      mountPath: /static
    - name: csi
      mountPath: /csi
  volumes:
  - name: static
    persistentVolumeClaim:
      claimName: migration-static-pvc
  - name: csi
    persistentVolumeClaim:
      claimName: migration-csi-pvc
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: migration-nginx
  namespace: migration-apps
spec:
  replicas: 1
  selector:
    matchLabels:
      app: migration-nginx
  template:
    metadata:
      labels:
        app: migration-nginx
    spec:
      containers:
      - name: nginx
        image: nginx:1.27.5
        ports:
        - containerPort: 80
---
apiVersion: v1
kind: Service
metadata:
  name: migration-nginx
  namespace: migration-apps
spec:
  selector:
    app: migration-nginx
  ports:
  - port: 80
    targetPort: 80
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: migration-nginx
  namespace: migration-apps
spec:
  ingressClassName: traefik
  rules:
  - host: migration.test
    http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service:
            name: migration-nginx
            port:
              number: 80
---
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: migration-selfsigned
spec:
  selfSigned: {}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: migration-test
  namespace: migration-apps
spec:
  secretName: migration-test-tls
  issuerRef:
    name: migration-selfsigned
    kind: ClusterIssuer
  dnsNames:
  - migration.test
YAML
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-static-pvc --timeout=5m
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-csi-pvc --timeout=10m
    kubectl wait -n migration-apps --for=condition=Ready pod/migration-seed --timeout=10m
    kubectl rollout status -n migration-apps deployment/migration-nginx --timeout=5m
    kubectl wait -n cert-manager --for=condition=Available deployment/cert-manager --timeout=5m
    kubectl wait -n cert-manager --for=condition=Available deployment/cert-manager-webhook --timeout=5m
    kubectl wait -n cert-manager --for=condition=Available deployment/cert-manager-cainjector --timeout=5m
    kubectl wait --for=condition=Established crd/certificates.cert-manager.io --timeout=2m
    kubectl wait --for=condition=Established crd/clusterissuers.cert-manager.io --timeout=2m
    kubectl wait -n migration-apps --for=condition=Ready certificate/migration-test --timeout=5m
    kubectl delete pod -n migration-apps migration-seed --wait=true
}

verify_stage() {
    local stage="$1"
    CURRENT_KUBECONFIG="$2"
    export KUBECONFIG="$CURRENT_KUBECONFIG"
    echo "Verifying stage=$stage distro=$SOURCE_DIST kubeconfig=$CURRENT_KUBECONFIG"
    kubectl wait --for=condition=Ready node --all --timeout=5m
    if [[ -n "${NODEMIGRATE_EXPECTED_NODES:-}" ]]; then
        local expected_nodes actual_nodes
        expected_nodes="$(tr ',' '\n' <<<"$NODEMIGRATE_EXPECTED_NODES" | LC_ALL=C sort | paste -sd, -)"
        actual_nodes="$(kubectl get nodes -o json | jq -r '[.items[].metadata.name] | sort | join(",")')"
        [[ "$actual_nodes" == "$expected_nodes" ]] || {
            echo "node membership differs at stage $stage: expected=$expected_nodes actual=$actual_nodes" >&2
            return 1
        }
        local expected_control_planes expected_workers actual_control_planes actual_workers
        expected_control_planes="${NODEMIGRATE_EXPECTED_CONTROL_PLANES:-3}"
        expected_workers="${NODEMIGRATE_EXPECTED_WORKERS:-2}"
        read -r actual_control_planes actual_workers < <(kubectl get nodes -o json | jq -r '
          [
            ([.items[] | select((.metadata.labels // {}) | (has("node-role.kubernetes.io/control-plane") or has("node-role.kubernetes.io/master")))] | length),
            ([.items[] | select(((.metadata.labels // {}) | (has("node-role.kubernetes.io/control-plane") or has("node-role.kubernetes.io/master"))) | not)] | length)
          ] | @tsv
        ')
        [[ "$actual_control_planes" == "$expected_control_planes" && "$actual_workers" == "$expected_workers" ]] || {
            echo "node roles differ at stage $stage: expected control-planes=$expected_control_planes workers=$expected_workers actual control-planes=$actual_control_planes workers=$actual_workers" >&2
            return 1
        }
    fi
    kubectl get nodes -o json | jq -e '
      all(.items[];
        .metadata.labels["operator.example/pool"] == "blue" and
        .metadata.annotations["nodemigrate.io/source-uid"] == "operator-node-value" and
        any(.spec.taints[]?; .key == "operator.example/dedicated" and .value == "migration" and .effect == "PreferNoSchedule")
      )
    ' >/dev/null || {
        echo "nodemigrate changed user Node labels, annotations, or taints at stage $stage" >&2
        return 1
    }
    kubectl rollout status daemonset/cilium -n kube-system --timeout=5m
    kubectl get crd ciliumendpoints.cilium.io
    kubectl rollout status -n migration-apps deployment/migration-nginx --timeout=5m
    local preserved_annotation
    local configmap_json
    configmap_json="$(kubectl get configmap migration-user-metadata -n migration-apps -o json)"
    preserved_annotation="$(jq -r '.metadata.annotations["nodemigrate.io/source-uid"]' <<<"$configmap_json")"
    [[ "$preserved_annotation" == user-value ]] || {
        echo "nodemigrate changed migration-user-metadata annotation at stage $stage" >&2
        return 1
    }
    [[ "$(jq -r '.data["migration-marker"]' <<<"$configmap_json")" == user-config-data ]] || {
        echo "nodemigrate changed migration-user-metadata data at stage $stage" >&2
        return 1
    }
    kubectl wait -n migration-apps --for=condition=Ready certificate/migration-test --timeout=5m
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-static-pvc --timeout=5m
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-csi-pvc --timeout=5m

    cat > /tmp/nodemigrate-verify-pod.yaml <<'YAML'
apiVersion: v1
kind: Pod
metadata:
  name: migration-data-check
  namespace: migration-apps
spec:
  restartPolicy: Never
  containers:
  - name: verify
    image: busybox:1.36.1
    command: [sh, -c, 'test "$(cat /static/marker)" = static-persistent-data && test "$(cat /csi/marker)" = csi-persistent-data']
    volumeMounts:
    - name: static
      mountPath: /static
    - name: csi
      mountPath: /csi
  volumes:
  - name: static
    persistentVolumeClaim:
      claimName: migration-static-pvc
  - name: csi
    persistentVolumeClaim:
      claimName: migration-csi-pvc
YAML
    kubectl delete pod -n migration-apps migration-data-check --ignore-not-found --wait=true
    kubectl apply -f /tmp/nodemigrate-verify-pod.yaml
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Succeeded pod/migration-data-check --timeout=5m
    kubectl delete pod -n migration-apps migration-data-check --wait=true

    kubectl delete pod -n traefik migration-route-check --ignore-not-found --wait=true
    kubectl port-forward -n traefik svc/traefik 18080:80 >/tmp/traefik-port-forward.log 2>&1 &
    local port_forward_pid=$!
    trap 'kill "$port_forward_pid" 2>/dev/null || true' RETURN
    local response=""
    for _ in $(seq 1 30); do
        response="$(curl -fsS -H 'Host: migration.test' http://127.0.0.1:18080/ 2>/dev/null || true)"
        [[ "$response" == *"Welcome to nginx!"* ]] && break
        sleep 2
    done
    [[ "$response" == *"Welcome to nginx!"* ]] || {
        cat /tmp/traefik-port-forward.log >&2 || true
        echo "Traefik did not route to the nginx workload at stage $stage" >&2
        return 1
    }
    kill "$port_forward_pid" 2>/dev/null || true
    trap - RETURN
    kubectl get deploy,svc,ingress,certificate,pv,pvc -A -o wide
    capture_semantic_checkpoint "$stage"
    echo "PASS stage=$stage"
}

capture_semantic_checkpoint() {
    local stage="$1"
    local stage_dir="$CHECKPOINT_DIR/$stage"
    mkdir -p "$stage_dir"
    chmod 0700 "$CHECKPOINT_DIR" "$stage_dir"

    capture_migratable_api_objects "$stage_dir/migratable-objects.jsonl"

    # Keep only stable, user-visible state. API-assigned UIDs, resource
    # versions, managed fields, and status timestamps naturally change during
    # a round trip; resource specs, bindings, workload identity, and readiness
    # must still match. Secret data is hashed in a pipe and never written to a
    # checkpoint or the action log.
    kubectl get nodes -o json | jq -S '
      [.items[] | {
        name: .metadata.name,
        controlPlane: ((.metadata.labels // {}) | (has("node-role.kubernetes.io/control-plane") or has("node-role.kubernetes.io/master"))),
        worker: (((.metadata.labels // {}) | (has("node-role.kubernetes.io/worker"))) or
          ((.metadata.labels // {}) | (has("node-role.kubernetes.io/control-plane") or has("node-role.kubernetes.io/master")) | not)),
        migrationLabel: ((.metadata.labels // {})["operator.example/pool"] // ""),
        migrationAnnotation: ((.metadata.annotations // {})["nodemigrate.io/source-uid"] // ""),
        migrationTaint: ([.spec.taints[]? | select(.key == "operator.example/dedicated")] | sort_by(.effect, .key, .value)),
        ready: ([.status.conditions[]? | select(.type == "Ready" and .status == "True")] | length == 1)
      }] | sort_by(.name)
    ' > "$stage_dir/nodes.json"

    kubectl get configmap,deployment,service,ingress,pvc -n migration-apps -o json \
      | canonicalize_api_list > "$stage_dir/application.json"
    kubectl get pv -o json \
      | jq -S '[.items[] | select(.spec.claimRef.namespace == "migration-apps") | {
          kind, name: .metadata.name, labels: (.metadata.labels // {}),
          annotations: (.metadata.annotations // {}),
          spec: (.spec | if .claimRef then .claimRef |= del(.uid) else . end)
        }] | sort_by(.name)' > "$stage_dir/persistent-volumes.json"
    kubectl get certificate -n migration-apps migration-test -o json \
      | canonicalize_api_object > "$stage_dir/certificate.json"
    kubectl get clusterissuer migration-selfsigned -o json \
      | canonicalize_api_object > "$stage_dir/issuer.json"

    for namespace in cert-manager traefik; do
        kubectl get deployment -n "$namespace" -o json \
          | canonicalize_api_list > "$stage_dir/$namespace-deployments.json"
    done
    kubectl get daemonset cilium -n kube-system -o json | jq -S '
      {
        name: .metadata.name,
        images: ([.spec.template.spec.containers[].image] | sort),
        desired: .status.desiredNumberScheduled,
        ready: .status.numberReady,
        updated: .status.updatedNumberScheduled
      }
    ' > "$stage_dir/cilium.json"
    kubectl get crd ciliumendpoints.cilium.io certificates.cert-manager.io \
      clusterissuers.cert-manager.io -o json \
      | jq -S '[.items[] | {
          name: .metadata.name,
          group: .spec.group,
          scope: .spec.scope,
          names: .spec.names,
          versions: [.spec.versions[] | {name, served, storage, schema}]
        }] | sort_by(.name)' > "$stage_dir/required-crds.json"
    kubectl get storageclass csi-hostpath-sc -o json \
      | canonicalize_api_object > "$stage_dir/storageclass.json"
    kubectl get secret migration-test-tls -n migration-apps -o json \
      | jq -S -c '.data // {}' | sha256sum | awk '{print $1}' \
      > "$stage_dir/certificate-secret.sha256"
    kubectl get configmap migration-user-metadata -n migration-apps -o json \
      | jq -S -c '.data // {}' | sha256sum | awk '{print $1}' \
      > "$stage_dir/user-configmap-data.sha256"

    jq -S -n \
      --slurpfile nodes "$stage_dir/nodes.json" \
      --slurpfile app "$stage_dir/application.json" \
      --slurpfile pvs "$stage_dir/persistent-volumes.json" \
      --slurpfile certificate "$stage_dir/certificate.json" \
      --slurpfile issuer "$stage_dir/issuer.json" \
      --slurpfile cert_manager "$stage_dir/cert-manager-deployments.json" \
      --slurpfile traefik "$stage_dir/traefik-deployments.json" \
      --slurpfile cilium "$stage_dir/cilium.json" \
      --slurpfile crds "$stage_dir/required-crds.json" \
      --slurpfile storageclass "$stage_dir/storageclass.json" \
      --slurpfile migratable_objects "$stage_dir/migratable-objects.jsonl" \
      '{nodes:$nodes[0], application:$app[0], persistentVolumes:$pvs[0],
        certificate:$certificate[0], issuer:$issuer[0],
        certManager:$cert_manager[0], traefik:$traefik[0], cilium:$cilium[0],
        requiredCrds:$crds[0], storageClass:$storageclass[0],
        migratableObjects:$migratable_objects}' \
      > "$stage_dir/semantic-state.json"
    chmod 0600 "$stage_dir"/*.json "$stage_dir"/*.jsonl "$stage_dir"/*.sha256
}

capture_migratable_api_objects() {
    local output="$1"
    local resources resource list_json object_json normalized identity digest
    resources="$(kubectl api-resources --verbs=list -o name | LC_ALL=C sort -u)"
    [[ -n "$resources" ]] || {
        echo "Kubernetes API discovery returned no listable resources" >&2
        return 1
    }
    : > "$output"
    while IFS= read -r resource; do
        [[ -n "$resource" ]] || continue
        list_json="$(kubectl get "$resource" --all-namespaces --chunk-size=500 -o json)"
        while IFS= read -r object_json; do
            normalized="$(jq -cS -f "$ROOT/.github/scripts/nodemigrate-snapshot-normalize.jq" \
                <<< "$object_json")"
            [[ -n "$normalized" ]] || continue
            identity="$(jq -cS '{apiVersion, kind, namespace: (.metadata.namespace // ""), name: .metadata.name}' <<< "$normalized")"
            digest="$(printf '%s' "$normalized" | sha256sum | awk '{print $1}')"
            jq -cS -n --argjson identity "$identity" --arg digest "$digest" \
                '{identity: $identity, sha256: $digest}' >> "$output"
        done < <(jq -c '.items[]' <<< "$list_json")
    done <<< "$resources"
    LC_ALL=C sort -o "$output" "$output"
}

canonicalize_api_list() {
    jq -S '[.items[] | {
      apiVersion, kind, name: .metadata.name,
      namespace: (.metadata.namespace // ""),
      labels: (.metadata.labels // {}),
      annotations: (.metadata.annotations // {}),
      ownerReferences: [(.metadata.ownerReferences // [])[] | {apiVersion, kind, name, controller}],
      spec: (.spec // {})
    }] | sort_by(.kind, .namespace, .name)'
}

canonicalize_api_object() {
    jq -S '{apiVersion, kind, name: .metadata.name,
      namespace: (.metadata.namespace // ""),
      labels: (.metadata.labels // {}),
      annotations: (.metadata.annotations // {}),
      spec: (.spec // {})}'
}

assert_round_trip_unchanged() {
    local initial="$CHECKPOINT_DIR/source"
    local returned="$CHECKPOINT_DIR/returned"
    if ! cmp -s "$initial/semantic-state.json" "$returned/semantic-state.json"; then
        echo "Returned Kubernetes semantic state differs from the source checkpoint" >&2
        diff -u "$initial/semantic-state.json" "$returned/semantic-state.json" || true
        return 1
    fi
    if ! cmp -s "$initial/certificate-secret.sha256" "$returned/certificate-secret.sha256"; then
        echo "Certificate secret contents changed during the migration round trip" >&2
        return 1
    fi
    if ! cmp -s "$initial/user-configmap-data.sha256" "$returned/user-configmap-data.sha256"; then
        echo "ConfigMap contents changed during the migration round trip" >&2
        return 1
    fi
    echo "PASS: returned semantic state, certificate secret, and ConfigMap contents match the source checkpoint"
}

assert_migratable_api_state_unchanged() {
    local before="$CHECKPOINT_DIR/$1/migratable-objects.jsonl"
    local after="$CHECKPOINT_DIR/$2/migratable-objects.jsonl"
    if ! cmp -s "$before" "$after"; then
        echo "Migratable Kubernetes API objects differ between stages $1 and $2" >&2
        diff -u "$before" "$after" || true
        return 1
    fi
    echo "PASS: all migratable Kubernetes API object fingerprints match between $1 and $2"
}

main() {
    need_root
    install_tools
    install_source
    install_cilium
    install_hostpath_driver
    install_workloads
    verify_stage source "$SOURCE_KUBECONFIG"

    export NOTK8S_COMBINED_PREBUILT="$NK"
    export NODEBOOTSTRAP_COMBINED_SELF="$NK"
    export NOTK8S_BUILD_LAYOUT=combined
    export NODEBOOTSTRAP_REPO_ROOT="$ROOT"
    local nodestore_kubeconfig=/etc/nodebootstrap/admin.kubeconfig
    echo "Migrating $SOURCE_DIST -> nodestore"
    NODEMIGRATE_SOURCE_KUBECONFIG="$SOURCE_KUBECONFIG" \
    NODEMIGRATE_DESTINATION_KUBECONFIG="$nodestore_kubeconfig" \
        "$MIGRATE" to=nodestore "from=$SOURCE_DIST"
    verify_stage nodestore "$nodestore_kubeconfig"
    assert_migratable_api_state_unchanged source nodestore

    echo "Migrating nodestore -> $SOURCE_DIST"
    NODEMIGRATE_REPLACE_NODE=true \
    NODEMIGRATE_SOURCE_KUBECONFIG="$nodestore_kubeconfig" \
    NODEMIGRATE_DESTINATION_KUBECONFIG="$SOURCE_KUBECONFIG" \
        "$MIGRATE" "to=$SOURCE_DIST" from=nodestore
    verify_stage returned "$SOURCE_KUBECONFIG"
    assert_round_trip_unchanged
}

if [[ "$LIBRARY_MODE" != true ]]; then
    main "$@"
fi
