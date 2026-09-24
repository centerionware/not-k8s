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
        --tmpfs /run \
        --tmpfs /run/lock \
        --tmpfs /sys/fs/bpf \
        --mount type=bind,src=/sys/fs/cgroup,dst=/sys/fs/cgroup \
        --volume "$volume:/var/lib/nodemigrate-volume" \
        "$IMAGE" >/dev/null
done

wait_systemd() {
    local container="$1" state=""
    for _ in $(seq 1 60); do
        state="$(docker exec "$container" systemctl is-system-running 2>/dev/null || true)"
        if [[ "$state" == running || "$state" == degraded ]]; then
            return 0
        fi
        sleep 1
    done
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
        ctr plugins ls | grep -Eq "io.containerd.grpc.v1[[:space:]]+cri[[:space:]].*[[:space:]]ok$"
        unshare --net true
        test -r /sys/kernel/btf/vmlinux
        mount -t bpf bpffs /sys/fs/bpf
        printf "%s\n" "$HOSTNAME" > /var/lib/nodemigrate-volume/node-identity
        printf "%s\n" "$HOSTNAME" > "/etc/nodemigrate-probe-${HOSTNAME}"
    ' || fail "$node failed CRI, namespace, BTF, bpffs, or storage checks"
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

echo "PASS: five Docker nodes have distinct namespaces, CRI and BPF support, separate persistent volumes, inter-node reachability, and single-node failure isolation"
echo "NOTE: this preflight does not establish Kubernetes, Cilium datapath, or migration parity; those require the five-node migration lane"
