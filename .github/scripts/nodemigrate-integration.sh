#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="${GITHUB_WORKSPACE:-$(git rev-parse --show-toplevel)}"
BIN_DIR="$ROOT/target/release"
NK="$BIN_DIR/notk8s"
MIGRATE="$BIN_DIR/nodemigrate"
SOURCE_DIST="${NODEMIGRATE_SOURCE_DIST:?set NODEMIGRATE_SOURCE_DIST to k3s or kubernetes}"
LOG="${NODEMIGRATE_TEST_LOG:-/tmp/nodemigrate-integration.log}"
WORK="/var/lib/nodemigrate-ci"
STATIC_PATH="$WORK/static-volume"
SOURCE_KUBECONFIG=""
CURRENT_KUBECONFIG=""

exec > >(tee -a "$LOG") 2>&1

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
trap diagnostics EXIT

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
    helm repo add cilium https://helm.cilium.io/ --force-update
    helm repo update cilium
    helm upgrade --install cilium cilium/cilium \
        --version "$version" --namespace kube-system \
        --set ipam.mode=kubernetes \
        --set kubeProxyReplacement=false \
        --set operator.replicas=1 \
        --set k8sServiceHost=127.0.0.1 \
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
    kubectl apply -f - <<'YAML'
apiVersion: v1
kind: Namespace
metadata:
  name: migration-apps
---
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
---
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
    kubectl rollout status daemonset/cilium -n kube-system --timeout=5m
    kubectl get crd ciliumendpoints.cilium.io
    kubectl rollout status -n migration-apps deployment/migration-nginx --timeout=5m
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
    echo "PASS stage=$stage"
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

    echo "Migrating nodestore -> $SOURCE_DIST"
    NODEMIGRATE_SOURCE_KUBECONFIG="$nodestore_kubeconfig" \
    NODEMIGRATE_DESTINATION_KUBECONFIG="$SOURCE_KUBECONFIG" \
        "$MIGRATE" "to=$SOURCE_DIST" from=nodestore
    verify_stage returned "$SOURCE_KUBECONFIG"
}

main "$@"
