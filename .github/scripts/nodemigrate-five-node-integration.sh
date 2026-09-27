#!/usr/bin/env bash
set -Eeuo pipefail

NODE_ROOT="${NODEMIGRATE_NODE_ROOT:-/workspace/not-k8s}"
SUFFIX="${NODEMIGRATE_DOCKER_SUFFIX:?missing node container suffix}"
IMAGE="${NODEMIGRATE_NODE_IMAGE:?missing node image}"
CILIUM_KPR="${NODEMIGRATE_CILIUM_KPR:-false}"
NODES=(cp-1 cp-2 cp-3 worker-1 worker-2)

container() { printf '%s-%s' "$1" "$SUFFIX"; }
node() { docker exec "$(container "$1")" "$@"; }
node_env() { docker exec "$(container "$1")" env "$@"; }

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

failure_diagnostics() {
    local status=$?
    trap - EXIT
    if ((status != 0)); then
        echo "Five-node migration failed; collecting diagnostics before Docker cleanup"
        for host in "${NODES[@]}"; do
            local current="$(container "$host")"
            echo "Diagnostics for $host ($current)"
            docker inspect --format 'state={{.State.Status}} exit={{.State.ExitCode}} error={{.State.Error}}' "$current" || true
            docker exec "$current" bash -c '
                systemctl status --no-pager --full kubelet containerd nodestore nodelet nodeapiserver || true
                journalctl -u kubelet -u containerd -u nodestore -u nodelet -u nodeapiserver -n 160 --no-pager || true
                ctr -n k8s.io tasks ls || true
                ctr -n k8s.io containers ls || true
            ' || true
            docker logs --tail 120 "$current" || true
        done
        node cp-1 env KUBECONFIG=/etc/kubernetes/admin.conf kubectl get nodes -o wide || true
        node cp-1 env KUBECONFIG=/etc/kubernetes/admin.conf kubectl get pods -A -o wide || true
        node cp-1 env KUBECONFIG=/etc/kubernetes/admin.conf \
            kubectl get events -A --sort-by=.lastTimestamp | tail -n 120 || true
        node cp-1 env KUBECONFIG=/etc/nodebootstrap/admin.kubeconfig \
            kubectl get nodes -o wide || true
    fi
    exit "$status"
}
trap failure_diagnostics EXIT

copy_tree() {
    local source_node="$1" source_path="$2" target_node="$3" target_path="$4"
    node "$target_node" mkdir -p "$target_path"
    node "$source_node" tar -C "$source_path" -cf - . \
        | docker exec -i "$(container "$target_node")" tar -C "$target_path" -xf -
    node "$target_node" find "$target_path" -type d -exec chmod 0700 '{}' +
    node "$target_node" find "$target_path" -type f -exec chmod 0600 '{}' +
}

copy_file() {
    local source_node="$1" source_path="$2" target_node="$3" target_path="$4"
    docker cp "$(container "$source_node"):$source_path" \
        "$(container "$target_node"):$target_path" >/dev/null
}

export_dir_from() {
    local output="$1" path
    path="$(sed -nE \
        -e 's/^Migration completed.*Export retained at (.+)$/\1/p' \
        -e 's/^Protected API object export saved at (.+)$/\1/p' \
        <<<"$output" | tail -n 1)"
    [[ -n "$path" ]] || fail "migration output did not identify its retained API export"
    printf '%s' "$path"
}

run_migration() {
    local host="$1" source_kubeconfig="$2" destination_kubeconfig="$3"
    local extra_env_name="$4"
    shift 4
    local -n extra_env="$extra_env_name"
    local output
    local -a env=(
        "PATH=$NODE_ROOT/target/release:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        "NOTK8S_COMBINED_PREBUILT=$NODE_ROOT/target/release/notk8s"
        "NODEBOOTSTRAP_COMBINED_SELF=$NODE_ROOT/target/release/notk8s"
        "NOTK8S_BUILD_LAYOUT=combined"
        "NODEBOOTSTRAP_REPO_ROOT=$NODE_ROOT"
        "NODEMIGRATE_SOURCE_DIST=kubernetes"
        "NODEMIGRATE_SOURCE_KUBECONFIG=$source_kubeconfig"
        "NODEMIGRATE_DESTINATION_KUBECONFIG=$destination_kubeconfig"
        "NODEMIGRATE_REPLACE_NODE=true"
    )
    env+=("${extra_env[@]}")
    echo "Running on $host: nodemigrate $*"
    if output="$(node_env "$host" "${env[@]}" "$NODE_ROOT/target/release/nodemigrate" "$@" 2>&1)"; then
        printf '%s\n' "$output" >&2
    else
        local status=$?
        printf '%s\n' "$output" >&2
        fail "nodemigrate $* failed on $host with status $status"
    fi
    printf '%s' "$output"
}

wait_five_nodes() {
    local kubeconfig="$1"
    node cp-1 env KUBECONFIG="$kubeconfig" kubectl wait \
        --for=condition=Ready node --all --timeout=10m
    node cp-1 env KUBECONFIG="$kubeconfig" kubectl get nodes -o wide
    node cp-1 env KUBECONFIG="$kubeconfig" kubectl get nodes -o json | jq -e '
      (.items | length) == 5 and
      ([.items[] | select((.metadata.labels // {}) | has("node-role.kubernetes.io/control-plane"))] | length) == 3 and
      ([.items[] | select(((.metadata.labels // {}) | has("node-role.kubernetes.io/control-plane")) | not)] | length) == 2 and
      all(.items[]; any(.status.conditions[]?; .type == "Ready" and .status == "True"))
    ' >/dev/null
}

cp1="$(container cp-1)"
expected_image_id="$(docker image inspect --format '{{.Id}}' "$IMAGE")"
for host in "${NODES[@]}"; do
    actual_image_id="$(docker inspect --format '{{.Image}}' "$(container "$host")")"
    [[ "$actual_image_id" == "$expected_image_id" ]] \
        || fail "$host is not running the expected nodemigrate node image"
done
cp1_ip="$(docker inspect --format \
    "{{with index .NetworkSettings.Networks \"nodemigrate-probe-${SUFFIX}\"}}{{.IPAddress}}{{end}}" \
    "$cp1")"
[[ "$cp1_ip" =~ ^[0-9]+(\.[0-9]+){3}$ ]] || fail "cannot determine cp-1 address: $cp1_ip"
API_ENDPOINT="https://$cp1_ip:6443"
JOIN_ENDPOINT="https://$cp1_ip:2379"
PKI_DIR=/var/lib/nodemigrate-five-node/pki
SOURCE_EXPORT_REMOTE=/var/lib/nodemigrate-five-node/source-export
RETURN_EXPORT_REMOTE=/var/lib/nodemigrate-five-node/return-export
NO_EXTRA_ENV=()

echo "Preparing shared nodestore client and peer trust roots"
node cp-1 env PKI_DIR="$PKI_DIR" bash -ec '
    umask 077
    mkdir -p "$PKI_DIR/client" "$PKI_DIR/peer"
    openssl req -x509 -newkey rsa:2048 -nodes -days 2 \
        -subj /CN=nodemigrate-client-ca \
        -addext basicConstraints=critical,CA:TRUE \
        -addext keyUsage=critical,keyCertSign,cRLSign \
        -keyout "$PKI_DIR/client/ca.key" -out "$PKI_DIR/client/ca.crt" >/dev/null 2>&1
    openssl req -x509 -newkey rsa:2048 -nodes -days 2 \
        -subj /CN=nodemigrate-peer-ca \
        -addext basicConstraints=critical,CA:TRUE \
        -addext keyUsage=critical,keyCertSign,cRLSign \
        -keyout "$PKI_DIR/peer/ca.key" -out "$PKI_DIR/peer/ca.crt" >/dev/null 2>&1
    openssl req -new -newkey rsa:2048 -nodes -subj /CN=nodemigrate-ci-client \
        -keyout "$PKI_DIR/client/client.key" -out "$PKI_DIR/client/client.csr" >/dev/null 2>&1
    printf "extendedKeyUsage=clientAuth\nkeyUsage=digitalSignature,keyEncipherment\n" > "$PKI_DIR/client/client.ext"
    openssl x509 -req -in "$PKI_DIR/client/client.csr" \
        -CA "$PKI_DIR/client/ca.crt" -CAkey "$PKI_DIR/client/ca.key" -CAcreateserial \
        -days 2 -extfile "$PKI_DIR/client/client.ext" \
        -out "$PKI_DIR/client/client.crt" >/dev/null 2>&1
    for member in cp-1 cp-2 cp-3; do
        address="$(getent ahostsv4 "$member" | awk "NR == 1 { print \$1 }")"
        test -n "$address"
        for domain in client peer; do
            if [ "$domain" = client ]; then ca="$PKI_DIR/client"; else ca="$PKI_DIR/peer"; fi
            openssl req -new -newkey rsa:2048 -nodes -subj "/CN=$member" \
                -keyout "$ca/server-$member.key" -out "$ca/server-$member.csr" >/dev/null 2>&1
            if [ "$domain" = peer ]; then usage=serverAuth,clientAuth; else usage=serverAuth; fi
            printf "basicConstraints=critical,CA:FALSE\nextendedKeyUsage=$usage\nkeyUsage=digitalSignature,keyEncipherment\nsubjectAltName=DNS:$member,DNS:localhost,IP:127.0.0.1,IP:$address\n" \
                > "$ca/server-$member.ext"
            openssl x509 -req -in "$ca/server-$member.csr" \
                -CA "$ca/ca.crt" -CAkey "$ca/ca.key" -CAcreateserial \
                -days 2 -extfile "$ca/server-$member.ext" \
                -out "$ca/server-$member.crt" >/dev/null 2>&1
        done
    done
    chmod 0600 "$PKI_DIR"/client/*.key "$PKI_DIR"/peer/*.key
'

echo "Installing and validating the source fixture on cp-1"
node cp-1 env NODEMIGRATE_HOSTPATH_SETUP=/tmp/nodemigrate-hostpath-setup.sh \
    NODEMIGRATE_CILIUM_KPR="$CILIUM_KPR" \
    bash "$NODE_ROOT/.github/scripts/nodemigrate-five-node-fixture.sh" source

echo "Migrating cp-1 and importing the cluster API state into nodestore"
cp1_initial_env=(
    "NODEBOOTSTRAP_APISERVER_SERVER=$API_ENDPOINT"
    "NODEBOOTSTRAP_ADVERTISE_ADDRESS=$cp1_ip"
    "NODEBOOTSTRAP_PKI_DIR=/var/lib/nodebootstrap/pki"
    "NODEBOOTSTRAP_JOIN_CA_FILE=$PKI_DIR/client/ca.crt"
    "NODEBOOTSTRAP_JOIN_CERT_FILE=$PKI_DIR/client/client.crt"
    "NODEBOOTSTRAP_JOIN_KEY_FILE=$PKI_DIR/client/client.key"
    "NODESTORE_LISTEN=0.0.0.0:2379"
    "NODESTORE_PEER_LISTEN=0.0.0.0:2380"
    "NODESTORE_DATA_DIR=/var/lib/nodestore"
    "NODESTORE_MEMBER_ID=1"
    "NODESTORE_INITIAL_CLUSTER=1=https://cp-1:2380"
    "NODESTORE_ADVERTISE_PEER_URL=https://cp-1:2380"
    "NODESTORE_ADVERTISE_CLIENT_URL=$JOIN_ENDPOINT"
    "NODESTORE_CERT_FILE=$PKI_DIR/client/server-cp-1.crt"
    "NODESTORE_KEY_FILE=$PKI_DIR/client/server-cp-1.key"
    "NODESTORE_TRUSTED_CA_FILE=$PKI_DIR/client/ca.crt"
    "NODESTORE_CLIENT_CA_FILE=$PKI_DIR/client/ca.crt"
    "NODESTORE_CLIENT_CERT_FILE=$PKI_DIR/client/client.crt"
    "NODESTORE_CLIENT_KEY_FILE=$PKI_DIR/client/client.key"
    "NODESTORE_PEER_CERT_FILE=$PKI_DIR/peer/server-cp-1.crt"
    "NODESTORE_PEER_KEY_FILE=$PKI_DIR/peer/server-cp-1.key"
    "NODESTORE_PEER_TRUSTED_CA_FILE=$PKI_DIR/peer/ca.crt"
)
cp1_forward_output="$(run_migration cp-1 /etc/kubernetes/admin.conf /etc/nodebootstrap/admin.kubeconfig \
    cp1_initial_env to=nodestore from=kubernetes)"
SOURCE_EXPORT="$(export_dir_from "$cp1_forward_output")"
[[ "$SOURCE_EXPORT" == /var/lib/nodemigrate/exports/* ]] \
    || fail "cp-1 returned an unexpected protected export path: $SOURCE_EXPORT"

echo "Preparing cp-1 target kubeconfig and shared control-plane PKI"
node cp-1 kubectl config set-cluster default --server="$API_ENDPOINT" \
    --kubeconfig=/etc/nodebootstrap/admin.kubeconfig >/dev/null
for host in cp-2 cp-3; do
    copy_tree cp-1 /var/lib/nodebootstrap/pki "$host" /var/lib/nodebootstrap/pki
    copy_file cp-1 /etc/nodebootstrap/admin.kubeconfig "$host" /etc/nodebootstrap/admin.kubeconfig
    node "$host" chmod 0600 /etc/nodebootstrap/admin.kubeconfig
    node "$host" mkdir -p "$PKI_DIR/client" "$PKI_DIR/peer"
    for domain in client peer; do
        copy_file cp-1 "$PKI_DIR/$domain/ca.crt" "$host" "$PKI_DIR/$domain/ca.crt"
    done
    copy_file cp-1 "$PKI_DIR/client/client.crt" "$host" "$PKI_DIR/client/client.crt"
    copy_file cp-1 "$PKI_DIR/client/client.key" "$host" "$PKI_DIR/client/client.key"
    copy_file cp-1 "$PKI_DIR/client/server-$host.crt" "$host" "$PKI_DIR/client/server-$host.crt"
    copy_file cp-1 "$PKI_DIR/client/server-$host.key" "$host" "$PKI_DIR/client/server-$host.key"
    copy_file cp-1 "$PKI_DIR/peer/server-$host.crt" "$host" "$PKI_DIR/peer/server-$host.crt"
    copy_file cp-1 "$PKI_DIR/peer/server-$host.key" "$host" "$PKI_DIR/peer/server-$host.key"
    node "$host" chmod 0600 "$PKI_DIR/client/client.key" "$PKI_DIR/client/server-$host.key" "$PKI_DIR/peer/server-$host.key"
done
copy_tree cp-1 "$SOURCE_EXPORT" cp-2 "$SOURCE_EXPORT_REMOTE"
copy_tree cp-1 "$SOURCE_EXPORT" cp-3 "$SOURCE_EXPORT_REMOTE"
for host in worker-1 worker-2; do
    copy_file cp-1 /etc/nodebootstrap/admin.kubeconfig "$host" /etc/nodebootstrap/admin.kubeconfig
    node "$host" chmod 0600 /etc/nodebootstrap/admin.kubeconfig
    copy_tree cp-1 "$SOURCE_EXPORT" "$host" "$SOURCE_EXPORT_REMOTE"
done

cp_join_env=(
    "NODEBOOTSTRAP_APISERVER_SERVER=$API_ENDPOINT"
    "NODEBOOTSTRAP_PKI_DIR=/var/lib/nodebootstrap/pki"
    "NODEBOOTSTRAP_JOIN_ENDPOINT=$JOIN_ENDPOINT"
    "NODEBOOTSTRAP_JOIN_CA_FILE=$PKI_DIR/client/ca.crt"
    "NODEBOOTSTRAP_JOIN_CERT_FILE=$PKI_DIR/client/client.crt"
    "NODEBOOTSTRAP_JOIN_KEY_FILE=$PKI_DIR/client/client.key"
    "NODESTORE_LISTEN=0.0.0.0:2379"
    "NODESTORE_PEER_LISTEN=0.0.0.0:2380"
    "NODESTORE_DATA_DIR=/var/lib/nodestore"
    "NODESTORE_TRUSTED_CA_FILE=$PKI_DIR/client/ca.crt"
    "NODESTORE_CLIENT_CA_FILE=$PKI_DIR/client/ca.crt"
    "NODESTORE_CLIENT_CERT_FILE=$PKI_DIR/client/client.crt"
    "NODESTORE_CLIENT_KEY_FILE=$PKI_DIR/client/client.key"
    "NODESTORE_PEER_TRUSTED_CA_FILE=$PKI_DIR/peer/ca.crt"
)

for host in cp-2 cp-3; do
    peer_url="https://$host:2380"
    cp_env=("${cp_join_env[@]}")
    cp_env+=("NODEBOOTSTRAP_PEER_URL=$peer_url")
    cp_env+=("NODESTORE_CERT_FILE=$PKI_DIR/client/server-$host.crt")
    cp_env+=("NODESTORE_KEY_FILE=$PKI_DIR/client/server-$host.key")
    cp_env+=("NODESTORE_PEER_CERT_FILE=$PKI_DIR/peer/server-$host.crt")
    cp_env+=("NODESTORE_PEER_KEY_FILE=$PKI_DIR/peer/server-$host.key")
    cp_env+=("NODESTORE_ADVERTISE_PEER_URL=$peer_url")
    cp_env+=("NODESTORE_ADVERTISE_CLIENT_URL=https://$host:2379")
    cp_env+=("NODEMIGRATE_DESTINATION_KUBECONFIG=/etc/nodebootstrap/admin.kubeconfig")
    output="$(node_env "$host" "${cp_env[@]}" \
        PATH="$NODE_ROOT/target/release:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
        NOTK8S_COMBINED_PREBUILT="$NODE_ROOT/target/release/notk8s" \
        NODEBOOTSTRAP_COMBINED_SELF="$NODE_ROOT/target/release/notk8s" \
        NOTK8S_BUILD_LAYOUT=combined NODEBOOTSTRAP_REPO_ROOT="$NODE_ROOT" \
        NODEMIGRATE_SOURCE_DIST=kubernetes NODEMIGRATE_SOURCE_KUBECONFIG=/etc/kubernetes/admin.conf \
        NODEMIGRATE_DESTINATION_KUBECONFIG=/etc/nodebootstrap/admin.kubeconfig \
        NODEMIGRATE_REPLACE_NODE=true \
        "$NODE_ROOT/target/release/nodemigrate" to=nodestore from=kubernetes \
            skip-api-import=true "source-export=$SOURCE_EXPORT_REMOTE" 2>&1)" \
        || fail "control-plane join migration failed on $host"
    printf '%s\n' "$output"
done

worker_env=(
    "NODEBOOTSTRAP_JOIN_ENDPOINT=$JOIN_ENDPOINT"
    "NODEBOOTSTRAP_WORKER_KUBECONFIG=/etc/nodebootstrap/admin.kubeconfig"
    "NODEMIGRATE_DESTINATION_KUBECONFIG=/etc/nodebootstrap/admin.kubeconfig"
    "NODEMIGRATE_REPLACE_NODE=true"
)
for host in worker-1 worker-2; do
    output="$(node_env "$host" "${worker_env[@]}" \
        PATH="$NODE_ROOT/target/release:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
        NOTK8S_COMBINED_PREBUILT="$NODE_ROOT/target/release/notk8s" \
        NODEBOOTSTRAP_COMBINED_SELF="$NODE_ROOT/target/release/notk8s" \
        NOTK8S_BUILD_LAYOUT=combined NODEBOOTSTRAP_REPO_ROOT="$NODE_ROOT" \
        NODEMIGRATE_SOURCE_DIST=kubernetes NODEMIGRATE_SOURCE_KUBECONFIG=/etc/kubernetes/admin.conf \
        NODEMIGRATE_DESTINATION_KUBECONFIG=/etc/nodebootstrap/admin.kubeconfig \
        NODEMIGRATE_REPLACE_NODE=true \
        "$NODE_ROOT/target/release/nodemigrate" to=nodestore from=kubernetes \
            skip-api-import=true "source-export=$SOURCE_EXPORT_REMOTE" 2>&1)" \
        || fail "worker join migration failed on $host"
    printf '%s\n' "$output"
done

echo "Checking all five nodes and running the nodestore fixture checkpoint"
wait_five_nodes /etc/nodebootstrap/admin.kubeconfig
node cp-1 env NODEMIGRATE_HOSTPATH_SETUP=/tmp/nodemigrate-hostpath-setup.sh \
    NODEMIGRATE_CILIUM_KPR="$CILIUM_KPR" \
    bash "$NODE_ROOT/.github/scripts/nodemigrate-five-node-fixture.sh" nodestore

echo "Staging the retained Kubernetes control planes to re-form etcd quorum"
cp1_return_output="$(run_migration cp-1 /etc/nodebootstrap/admin.kubeconfig /etc/kubernetes/admin.conf \
    NO_EXTRA_ENV to=kubernetes from=nodestore stage-target=true)"
RETURN_EXPORT="$(export_dir_from "$cp1_return_output")"
[[ "$RETURN_EXPORT" == /var/lib/nodemigrate/exports/* ]] \
    || fail "cp-1 returned an unexpected protected return export path: $RETURN_EXPORT"
copy_tree cp-1 "$RETURN_EXPORT" cp-2 "$RETURN_EXPORT_REMOTE"
copy_tree cp-1 "$RETURN_EXPORT" cp-3 "$RETURN_EXPORT_REMOTE"

for host in cp-2 cp-3; do
    run_migration "$host" /etc/nodebootstrap/admin.kubeconfig /etc/kubernetes/admin.conf \
        NO_EXTRA_ENV to=kubernetes from=nodestore stage-target=true "source-export=$RETURN_EXPORT_REMOTE" >/dev/null
done

echo "Waiting for retained kubeadm quorum, then importing the protected API state"
for _ in $(seq 1 120); do
    if node cp-1 env KUBECONFIG=/etc/kubernetes/admin.conf \
        kubectl --request-timeout=5s get --raw=/readyz >/dev/null 2>&1; then
        break
    fi
    sleep 2
done
node cp-1 env KUBECONFIG=/etc/kubernetes/admin.conf \
    kubectl --request-timeout=20s get --raw=/readyz >/dev/null \
    || fail "retained kubeadm API did not recover after all three control planes were staged"
run_migration cp-1 /etc/nodebootstrap/admin.kubeconfig /etc/kubernetes/admin.conf \
    NO_EXTRA_ENV to=kubernetes from=nodestore "import-export=$RETURN_EXPORT" >/dev/null

echo "Completing fresh Node registration for retained control planes and workers"
for host in cp-2 cp-3 worker-1 worker-2; do
    run_migration "$host" /etc/nodebootstrap/admin.kubeconfig /etc/kubernetes/admin.conf \
        NO_EXTRA_ENV to=kubernetes from=nodestore skip-api-export=true >/dev/null
done
wait_five_nodes /etc/kubernetes/admin.conf
node cp-1 env NODEMIGRATE_HOSTPATH_SETUP=/tmp/nodemigrate-hostpath-setup.sh \
    NODEMIGRATE_CILIUM_KPR="$CILIUM_KPR" \
    bash "$NODE_ROOT/.github/scripts/nodemigrate-five-node-fixture.sh" returned

echo "PASS: upstream kubeadm 3-control-plane/2-worker cluster migrated to nodestore and returned with five Ready Nodes and matching fixture state"
