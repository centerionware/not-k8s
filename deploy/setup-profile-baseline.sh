#!/usr/bin/env bash
# Destructive provisioning, ONLY on a fresh disposable GitHub Actions runner.
# A full upstream baseline: nodebootstrap's upstream target replaces only the
# apiserver, so it is intentionally not used to represent a Go Kubernetes stack.
set -euo pipefail
[[ ${GITHUB_ACTIONS:-} == true && ${PROFILE_DISPOSABLE_RUNNER:-} == 1 ]] || {
    echo 'baseline installation requires an explicitly disposable Actions runner' >&2; exit 2;
}
backend=${1:?k8s or k3s required}
[[ "$backend" == k8s || "$backend" == k3s ]] || exit 2
version=v1.34.3
sudo swapoff -a
sudo modprobe overlay
sudo modprobe br_netfilter
sudo sysctl -w net.ipv4.ip_forward=1 net.bridge.bridge-nf-call-iptables=1
work=$(mktemp -d)
if [[ "$backend" == k3s ]]; then
    release="${version}+k3s1"
    base="https://github.com/k3s-io/k3s/releases/download/$release"
    curl -fL --retry 3 "$base/k3s" -o "$work/k3s"
    curl -fL --retry 3 "$base/sha256sum-amd64.txt" -o "$work/checksums"
    (cd "$work" && awk '$2 == "k3s" {print}' checksums > selected.sha256 && test -s selected.sha256 && sha256sum -c selected.sha256)
    sudo install -m 0755 "$work/k3s" /usr/local/bin/k3s
    # Use the bundled runtime, not the runner's Docker containerd.
    sudo tee /etc/systemd/system/k3s.service >/dev/null <<'UNIT'
[Unit]
Description=Disposable upstream profiling baseline
After=network-online.target
[Service]
Type=simple
ExecStart=/usr/local/bin/k3s server --cluster-cidr=10.42.0.0/16 --service-cidr=10.43.0.0/16 --disable=traefik,servicelb,local-storage,metrics-server --write-kubeconfig-mode=644
KillMode=process
Delegate=yes
Restart=on-failure
[Install]
WantedBy=multi-user.target
UNIT
    sudo systemctl daemon-reload
    sudo systemctl start k3s
    for attempt in {1..90}; do
        [[ -s /etc/rancher/k3s/k3s.yaml ]] && break
        sleep 2
    done
    export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
    # kubectl is installed by the caller so all clients use the same version.
else
    sudo apt-get update -qq
    sudo apt-get install -y -qq ca-certificates curl gpg conntrack socat
    sudo mkdir -p /etc/apt/keyrings
    curl -fsSL --retry 3 https://pkgs.k8s.io/core:/stable:/v1.34/deb/Release.key | gpg --dearmor > "$work/kubernetes.gpg"
    sudo install -m 0644 "$work/kubernetes.gpg" /etc/apt/keyrings/kubernetes.gpg
    echo 'deb [signed-by=/etc/apt/keyrings/kubernetes.gpg] https://pkgs.k8s.io/core:/stable:/v1.34/deb/ /' | sudo tee /etc/apt/sources.list.d/kubernetes.list >/dev/null
    sudo apt-get update -qq
    sudo apt-get install -y -qq kubelet=1.34.3-1.1 kubeadm=1.34.3-1.1
    # Fresh runner only: replace Docker's CRI-disabled runtime configuration.
    sudo systemctl stop docker docker.socket containerd
    containerd config default > "$work/containerd.toml"
    sed -i 's/SystemdCgroup = false/SystemdCgroup = true/' "$work/containerd.toml"
    sudo install -m 0644 "$work/containerd.toml" /etc/containerd/config.toml
    sudo systemctl restart containerd
    sudo systemctl enable --now kubelet
    sudo kubeadm init --kubernetes-version="$version" --pod-network-cidr=10.42.0.0/16 \
        --service-cidr=10.43.0.0/16 --cri-socket=unix:///run/containerd/containerd.sock
    sudo chmod 644 /etc/kubernetes/admin.conf
    export KUBECONFIG=/etc/kubernetes/admin.conf
    curl -fL --retry 3 https://github.com/flannel-io/flannel/releases/download/v0.27.4/kube-flannel.yml -o "$work/flannel.yml"
    sed -i 's@10.244.0.0/16@10.42.0.0/16@g' "$work/flannel.yml"
    kubectl apply -f "$work/flannel.yml"
    kubectl taint nodes --all node-role.kubernetes.io/control-plane- || exit 1
fi
echo "KUBECONFIG=$KUBECONFIG" >> "$GITHUB_ENV"
for attempt in {1..90}; do
    if kubectl get nodes -o name 2>/dev/null | grep -q '^node/'; then break; fi
    sleep 2
done
kubectl wait nodes --all --for=condition=Ready --timeout=240s
# k3s applies bundled addons asynchronously after its API becomes ready.
# rollout status fails immediately on NotFound; first wait for creation.
kubectl -n kube-system wait --for=create deployment/coredns --timeout=180s
kubectl -n kube-system rollout status deployment/coredns --timeout=240s
