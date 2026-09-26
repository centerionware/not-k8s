#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="${GITHUB_WORKSPACE:-$(git rev-parse --show-toplevel)}"
IMAGE="${NODEMIGRATE_NODE_IMAGE:?set NODEMIGRATE_NODE_IMAGE to the built node image}"
LOG="${NODEMIGRATE_PREFLIGHT_LOG:-/tmp/nodemigrate-docker-preflight.log}"
SUFFIX="${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-1}-$$"
NETWORK="nodemigrate-probe-${SUFFIX}"
NODES=(cp-1 cp-2 cp-3 worker-1 worker-2)
CONTAINERS=()
VOLUMES=()

mkdir -p "$(dirname "$LOG")"
exec > >(tee -a "$LOG") 2>&1

cleanup() {
    local status=$?
    trap - EXIT
    if ((${#CONTAINERS[@]})); then
        docker rm -f "${CONTAINERS[@]}" >/dev/null 2>&1 || true
    fi
    docker network rm "$NETWORK" >/dev/null 2>&1 || true
    if ((${#VOLUMES[@]})); then
        docker volume rm "${VOLUMES[@]}" >/dev/null 2>&1 || true
    fi
    exit "$status"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    return 1
}

docker info >/dev/null || fail "Docker daemon is not available"
docker image inspect "$IMAGE" >/dev/null || fail "node image $IMAGE is unavailable"
echo "Docker host memory and swap before kubelet setup"
free -h
swapon --show
kernel_config="/boot/config-$(uname -r)"
[[ -r "$kernel_config" ]] || fail "Docker host kernel config is unavailable at $kernel_config"
echo "Using Docker host kernel config $kernel_config"
sudo swapoff -a
if awk 'NR > 1 { active=1 } END { exit !active }' /proc/swaps; then
    cat /proc/swaps
    fail "Docker host swap is still active after swapoff"
fi
echo "Docker host swap is disabled for the kubelet simulation"
echo "Loading kernel modules on the Docker host for privileged node containers"
sudo modprobe overlay
sudo modprobe br_netfilter
docker network create --driver bridge "$NETWORK" >/dev/null

echo "Creating five privileged systemd node containers on Docker network $NETWORK"
for node in "${NODES[@]}"; do
    container="${node}-${SUFFIX}"
    volume="${container}-data"
    CONTAINERS+=("$container")
    VOLUMES+=("$volume")
    docker volume create "$volume" >/dev/null
    docker run --detach \
        --name "$container" \
        --hostname "$node" \
        --network "$NETWORK" \
        --network-alias "$node" \
        --privileged \
        --cgroupns=private \
        --tmpfs /run:rw,nosuid,nodev,mode=0755 \
        --tmpfs /run/lock:rw,nosuid,nodev,mode=0755 \
        --tmpfs /sys/fs/bpf \
        --volume /boot:/boot:ro \
        --volume /lib/modules:/lib/modules:ro \
        --volume "$volume:/var/lib/nodemigrate-volume" \
        "$IMAGE" >/dev/null
done

wait_systemd() {
    local container="$1" state=""
    for _ in $(seq 1 60); do
        if [[ "$(docker inspect --format '{{.State.Running}}' "$container" 2>/dev/null || true)" != true ]]; then
            echo "Container stopped before systemd became ready: $container"
            docker inspect --format 'state={{.State.Status}} exit={{.State.ExitCode}} error={{.State.Error}}' "$container" || true
            docker logs --tail 100 "$container" || true
            fail "node container $container stopped during startup"
        fi
        state="$(docker exec "$container" systemctl is-system-running 2>/dev/null || true)"
        if [[ "$state" == running || "$state" == degraded ]]; then
            return 0
        fi
        sleep 1
    done
    echo "Systemd readiness timed out in $container"
    docker inspect --format 'state={{.State.Status}} exit={{.State.ExitCode}} error={{.State.Error}}' "$container" || true
    docker exec "$container" sh -c 'ps -p 1 -o pid,comm,args; systemctl status --no-pager --full || true' || true
    docker logs --tail 100 "$container" || true
    fail "systemd did not reach running state in $container (state=$state)"
}

declare -A NETNS=()
declare -A MNTNS=()
declare -A MACHINE_ID=()
echo "Checking node, network, mount, CRI, BPF, and storage isolation"
for index in "${!NODES[@]}"; do
    node="${NODES[$index]}"
    container="${CONTAINERS[$index]}"
    wait_systemd "$container"
    docker exec "$container" systemctl start containerd
    docker exec "$container" systemctl is-active --quiet containerd || fail "$node containerd is inactive"
    docker exec "$container" sh -ec '
        ctr plugins ls | awk '\''$1 == "io.containerd.grpc.v1" && $2 == "cri" && $NF == "ok" { found=1 } END { exit !found }'\'' || {
            echo "FAIL: containerd CRI plugin is not loaded and healthy" >&2
            ctr plugins ls >&2 || true
            exit 1
        }
        unshare --net true || {
            echo "FAIL: network namespace creation is unavailable" >&2
            exit 1
        }
        test -r /sys/kernel/btf/vmlinux || {
            echo "FAIL: host kernel BTF is unavailable in the node container" >&2
            exit 1
        }
        mount -t bpf bpffs /sys/fs/bpf || {
            echo "FAIL: mounting a private bpffs is unavailable in the node container" >&2
            exit 1
        }
        mount --make-rshared /sys/fs/bpf || {
            echo "FAIL: making the private node bpffs mount shared is unavailable" >&2
            exit 1
        }
        findmnt -n -t bpf -o TARGET,PROPAGATION,FSTYPE --target /sys/fs/bpf
        bpffs_propagation="$(findmnt -n -t bpf -o PROPAGATION --target /sys/fs/bpf)"
        if [ "$bpffs_propagation" != shared ]; then
            echo "FAIL: topmost node bpffs mount is not shared for nested Cilium Pods" >&2
            grep " /sys/fs/bpf " /proc/self/mountinfo >&2 || true
            exit 1
        fi
        printf "%s\n" "$HOSTNAME" > /var/lib/nodemigrate-volume/node-identity || {
            echo "FAIL: node persistent volume is not writable" >&2
            exit 1
        }
        printf "%s\n" "$HOSTNAME" > "/etc/nodemigrate-probe-${HOSTNAME}" || {
            echo "FAIL: node root filesystem marker is not writable" >&2
            exit 1
        }
    ' || fail "$node failed a CRI, namespace, BTF, bpffs, or storage check; see the specific diagnostic above"
    docker cp "$ROOT/.github/nodemigrate/bpf-probe.c" "$container:/tmp/bpf-probe.c" >/dev/null
    docker exec "$container" sh -ec '
        clang -O2 -target bpf -c /tmp/bpf-probe.c -o /tmp/bpf-probe.o
        bpftool prog load /tmp/bpf-probe.o "/sys/fs/bpf/nodemigrate-${HOSTNAME}" type classifier
        bpftool prog show pinned "/sys/fs/bpf/nodemigrate-${HOSTNAME}" >/dev/null
    ' || fail "$node could not load and pin an eBPF classifier program"
    NETNS[$node]="$(docker exec "$container" readlink /proc/1/ns/net)"
    MNTNS[$node]="$(docker exec "$container" readlink /proc/1/ns/mnt)"
    MACHINE_ID[$node]="$(docker exec "$container" cat /etc/machine-id)"
    [[ -n "${MACHINE_ID[$node]}" ]] || fail "$node has no systemd machine identity"
    [[ "$(docker exec "$container" cat /var/lib/nodemigrate-volume/node-identity)" == "$node" ]] \
        || fail "$node did not read its own persistent data volume"
    echo "PASS: $node netns=${NETNS[$node]} mountns=${MNTNS[$node]} machine-id=${MACHINE_ID[$node]} cri=active bpf=loaded"
done

for left in "${NODES[@]}"; do
    for right in "${NODES[@]}"; do
        [[ "$left" == "$right" ]] && continue
        [[ "${NETNS[$left]}" != "${NETNS[$right]}" ]] || fail "$left and $right share a network namespace"
        [[ "${MNTNS[$left]}" != "${MNTNS[$right]}" ]] || fail "$left and $right share a mount namespace"
        [[ "${MACHINE_ID[$left]}" != "${MACHINE_ID[$right]}" ]] || fail "$left and $right share a systemd machine identity"
        docker exec "${left}-${SUFFIX}" test -f "/etc/nodemigrate-probe-${left}" \
            || fail "$left cannot read its own root filesystem marker"
        docker exec "${left}-${SUFFIX}" test ! -e "/etc/nodemigrate-probe-${right}" \
            || fail "$left can see the root filesystem marker belonging to $right"
        [[ "$(docker exec "${left}-${SUFFIX}" cat /var/lib/nodemigrate-volume/node-identity)" == "$left" ]] \
            || fail "$left does not retain an independent persistent volume"
        docker exec "${left}-${SUFFIX}" ping -c 1 -W 2 "$right" >/dev/null \
            || fail "$left cannot reach $right over the isolated Docker network"
        docker exec "${left}-${SUFFIX}" test ! -e "/sys/fs/bpf/nodemigrate-${right}" \
            || fail "$left can see the BPF pin belonging to $right"
    done
done

node_container() {
    printf '%s-%s' "$1" "$SUFFIX"
}

node_ip() {
    docker inspect --format \
        "{{with index .NetworkSettings.Networks \"$NETWORK\"}}{{.IPAddress}}{{end}}" \
        "$(node_container "$1")"
}

echo "Configuring kubeadm and containerd on five isolated nodes"
for node in "${NODES[@]}"; do
    container="$(node_container "$node")"
    docker exec "$container" bash -ec '
        echo "$HOSTNAME: writing kernel module and sysctl configuration"
        cat >/etc/modules-load.d/kubernetes.conf <<EOF
overlay
br_netfilter
EOF
        cat >/etc/sysctl.d/99-kubernetes.conf <<EOF
net.bridge.bridge-nf-call-iptables=1
net.bridge.bridge-nf-call-ip6tables=1
        net.ipv4.ip_forward=1
EOF
        echo "$HOSTNAME: applying sysctls"
        sysctl --system
        echo "$HOSTNAME: configuring and restarting containerd"
        mkdir -p /etc/containerd
        containerd config default >/etc/containerd/config.toml
        sed -i "/snapshotter =/s/overlayfs/native/" /etc/containerd/config.toml
        sed -i "s/SystemdCgroup = false/SystemdCgroup = true/" /etc/containerd/config.toml
        cat >>/etc/containerd/config.toml <<EOF

[[plugins."io.containerd.transfer.v1.local".unpack_config]]
platform = "linux/amd64"
snapshotter = "native"
EOF
        if ! grep -Eq "snapshotter =.*native" /etc/containerd/config.toml; then
            grep -n "snapshotter =" /etc/containerd/config.toml || true
            echo "FAIL: containerd config did not select the native snapshotter" >&2
            exit 1
        fi
        grep -n "snapshotter =.*native" /etc/containerd/config.toml
        unpack_header="[[plugins.\"io.containerd.transfer.v1.local\".unpack_config]]"
        if ! grep -Fq "$unpack_header" /etc/containerd/config.toml; then
            tail -n 16 /etc/containerd/config.toml
            echo "FAIL: containerd native snapshotter unpack configuration is missing" >&2
            exit 1
        fi
        unpack_config="$(grep -F -A2 "$unpack_header" /etc/containerd/config.toml)"
        printf '%s\n' "$unpack_config"
        grep -Fq "platform = \"linux/amd64\"" <<<"$unpack_config" || {
                echo "FAIL: containerd native snapshotter unpack platform is missing" >&2
                exit 1
            }
        grep -Fq "snapshotter = \"native\"" <<<"$unpack_config" || {
                echo "FAIL: containerd native snapshotter unpack mapping is missing" >&2
                exit 1
            }
        systemctl restart containerd
        echo "$HOSTNAME: enabling kubelet and checking Kubernetes tools"
        systemctl enable kubelet
        kubeadm version -o short
        kubelet --version
    '
done

cp1="$(node_container cp-1)"
cp1_ip="$(node_ip cp-1)"
echo "Initializing upstream Kubernetes control plane cp-1 at $cp1_ip"

collect_node_diagnostics() {
    local node container
    for node in "${NODES[@]}"; do
        container="$(node_container "$node")"
        echo "Failure diagnostics for $node"
        docker exec "$container" bash -c '
            systemctl status --no-pager --full kubelet containerd || true
            findmnt -n -o TARGET,PROPAGATION,FSTYPE --target /sys/fs/bpf || true
            grep " /sys/fs/bpf " /proc/self/mountinfo || true
            journalctl -u kubelet -u containerd -n 150 --no-pager || true
            ctr -n k8s.io tasks ls || true
            ctr -n k8s.io containers ls || true
        ' || true
        docker logs --tail 150 "$container" || true
    done
}

collect_cluster_diagnostics() {
    echo "Collecting Kubernetes, Cilium, and node runtime diagnostics"
    docker exec "$cp1" bash -c '
        export KUBECONFIG=/etc/kubernetes/admin.conf
        kubectl get nodes -o wide || true
        kubectl get pods -A -o wide || true
        kubectl get events -A --sort-by=.lastTimestamp | tail -n 120 || true
        for pod in $(kubectl get pods -n kube-system -l k8s-app=cilium -o name 2>/dev/null); do
            echo "Cilium Pod diagnostics: $pod"
            kubectl describe "$pod" -n kube-system || true
            kubectl logs "$pod" -n kube-system --all-containers --tail=120 || true
            kubectl logs "$pod" -n kube-system --all-containers --previous --tail=120 || true
        done
        for pod in $(kubectl get pods -n kube-system -l io.cilium/app=operator -o name 2>/dev/null); do
            echo "Cilium operator diagnostics: $pod"
            kubectl describe "$pod" -n kube-system || true
            kubectl logs "$pod" -n kube-system --all-containers --tail=120 || true
            kubectl logs "$pod" -n kube-system --all-containers --previous --tail=120 || true
        done
    ' || true
    collect_node_diagnostics
}

if ! docker exec "$cp1" kubeadm init \
    --kubernetes-version "$(docker exec "$cp1" kubeadm version -o short)" \
    --control-plane-endpoint=cp-1:6443 \
    --apiserver-advertise-address="$cp1_ip" \
    --pod-network-cidr=10.244.0.0/16 \
    --cri-socket=unix:///run/containerd/containerd.sock \
    --upload-certs; then
    echo "kubeadm init failed; collecting node diagnostics before cleanup"
    collect_node_diagnostics
    exit 1
fi

join_command="$(docker exec "$cp1" kubeadm token create --ttl 2h --print-join-command)"
certificate_key="$(docker exec "$cp1" kubeadm init phase upload-certs --upload-certs \
    | tail -n 1)"
[[ "$certificate_key" =~ ^[a-f0-9]{64}$ ]] \
    || fail "kubeadm did not produce a usable control-plane certificate key"
read -r -a join_args <<<"$join_command"
[[ "${join_args[0]:-}" == kubeadm && "${join_args[1]:-}" == join ]] \
    || fail "kubeadm returned an unexpected join command: $join_command"

echo "Joining cp-2 and cp-3 to the stacked-etcd control plane"
for node in cp-2 cp-3; do
    node_ip="$(node_ip "$node")"
    docker exec "$(node_container "$node")" "${join_args[@]}" \
        --control-plane \
        --certificate-key "$certificate_key" \
        --apiserver-advertise-address="$node_ip" \
        --cri-socket=unix:///run/containerd/containerd.sock
done

echo "Joining worker-1 and worker-2 to the upstream cluster"
for node in worker-1 worker-2; do
    docker exec "$(node_container "$node")" "${join_args[@]}" \
        --cri-socket=unix:///run/containerd/containerd.sock
done

echo "Installing Helm and Cilium in the five-node upstream cluster"
if ! docker exec "$cp1" bash -ec '
    curl -fsSL https://get.helm.sh/helm-v3.17.3-linux-amd64.tar.gz -o /tmp/helm.tgz
    tar -xzf /tmp/helm.tgz -C /tmp
    install -m0755 /tmp/linux-amd64/helm /usr/local/bin/helm
    export KUBECONFIG=/etc/kubernetes/admin.conf
    helm repo add cilium https://helm.cilium.io/ --force-update
    helm repo update cilium
    helm upgrade --install cilium cilium/cilium --version 1.20.2 \
        --namespace kube-system \
        --set ipam.mode=kubernetes \
        --set cni.binPath=/opt/cni/bin \
        --set kubeProxyReplacement=false \
        --set operator.replicas=1 \
        --set k8sServiceHost=cp-1 \
        --set k8sServicePort=6443 \
        --wait --timeout 10m
    kubectl rollout status daemonset/cilium -n kube-system --timeout=10m
    kubectl rollout status deployment/cilium-operator -n kube-system --timeout=10m
    kubectl wait --for=condition=Ready node --all --timeout=10m
    echo "Upstream versions:"
    kubeadm version -o short
    helm list -n kube-system --output json
    kubectl get nodes -o wide
    kubectl get nodes -o json | jq -e "
      (.items | length) == 5 and
      ([.items[] | select((.metadata.labels // {}) | has(\"node-role.kubernetes.io/control-plane\"))] | length) == 3 and
      ([.items[] | select(((.metadata.labels // {}) | has(\"node-role.kubernetes.io/control-plane\")) | not)] | length) == 2
    " >/dev/null
    kubectl get daemonset cilium -n kube-system -o json | jq -e "
      .status.desiredNumberScheduled == 5 and .status.numberReady == 5
    " >/dev/null
'; then
    echo "Cilium installation or readiness checks failed; collecting diagnostics"
    collect_cluster_diagnostics
    exit 1
fi

echo "Verifying the three-member etcd control plane survives cp-1 loss"
cp2="$(node_container cp-2)"
docker exec "$cp2" kubectl config set-cluster kubernetes \
    --server=https://cp-2:6443 --kubeconfig=/etc/kubernetes/admin.conf
docker stop --time 2 "$cp1" >/dev/null
docker exec "$cp2" env KUBECONFIG=/etc/kubernetes/admin.conf \
    kubectl --request-timeout=20s get --raw=/readyz >/dev/null
docker exec "$cp2" env KUBECONFIG=/etc/kubernetes/admin.conf \
    kubectl --request-timeout=20s get nodes -o wide
docker start "$cp1" >/dev/null
wait_systemd "$cp1"
for _ in $(seq 1 120); do
    if docker exec "$cp2" env KUBECONFIG=/etc/kubernetes/admin.conf \
        kubectl --request-timeout=5s wait --for=condition=Ready node --all --timeout=5s >/dev/null 2>&1; then
        break
    fi
    sleep 2
done
docker exec "$cp2" env KUBECONFIG=/etc/kubernetes/admin.conf \
    kubectl --request-timeout=20s get nodes -o wide
docker exec "$cp2" env KUBECONFIG=/etc/kubernetes/admin.conf \
    kubectl get nodes -o json | jq -e '
      (.items | length) == 5 and all(.items[]; any(.status.conditions[]?; .type == "Ready" and .status == "True"))
    ' >/dev/null || fail "the five-node upstream cluster did not recover after cp-1 restart"

echo "Stopping cp-1 to verify failure isolation"
docker stop --time 2 "cp-1-${SUFFIX}" >/dev/null
for node in cp-2 cp-3 worker-1 worker-2; do
    docker inspect --format '{{.State.Running}}' "${node}-${SUFFIX}" | grep -qx true \
        || fail "$node stopped when cp-1 was stopped"
done
docker start "cp-1-${SUFFIX}" >/dev/null
wait_systemd "cp-1-${SUFFIX}"
docker exec "cp-1-${SUFFIX}" systemctl is-active --quiet containerd \
    || fail "cp-1 did not recover its CRI after restart"

echo "PASS: five Docker nodes ran a kubeadm 3-control-plane/2-worker cluster with Cilium, survived control-plane loss, and recovered all Nodes"
echo "PASS: Docker isolation checks confirmed distinct namespaces, CRI/BPF support, separate storage, and inter-node reachability"
echo "NOTE: this preflight validates the five-node Kubernetes/Cilium simulation only; nodemigrate runtime and migration parity remain unverified"
