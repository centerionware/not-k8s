# lib/test/netns.sh — a real multi-node nodestore cluster on one host.
#
# Each member gets its own network namespace, its own veth into a shared
# bridge, and its own IP. They are separate processes talking over a real
# network stack: real TCP, real gRPC, real packet loss when we cause some.
#
# # Why namespaces rather than just different ports
#
# Ports on loopback would test the code and almost nothing else. The failures
# worth finding in a consensus implementation are network failures, and on
# loopback there is no interface to add latency to, nothing to drop packets
# with, and no way to partition one member from another without also
# partitioning it from itself. With a veth per member, `tc netem` and
# `iptables` operate on exactly the link between two members — which is what
# makes "the leader is unreachable but still running" expressible at all, and
# that case is precisely where split-brain bugs live.
#
# Everything here needs root (`ip netns`), which this suite already has.

NETNS_PREFIX="${NETNS_PREFIX:-nsdt}"
NETNS_BRIDGE="${NETNS_BRIDGE:-${NETNS_PREFIX}br0}"
NETNS_SUBNET="${NETNS_SUBNET:-10.177.0}"
NETNS_PEER_PORT=2380
NETNS_CLIENT_PORT=2379

# netns_supported — whether this host can run these tests at all.
netns_supported() {
    [[ "$(id -u)" -eq 0 ]] || return 1
    NETNS_MISSING_TOOL=""
    # Every tool the helpers below reach for, checked up front. Each of those
    # helpers redirects its own errors to /dev/null and returns 0 regardless,
    # so a missing tool does not produce a failure — it produces a *false
    # pass*: netns_partition on a host with no iptables reports success and
    # installs no rule, and the partition test then asserts partition
    # behaviour against a fully connected cluster. Same for netns_add_latency
    # without tc. Checking here turns that into a clean skip.
    local tool
    for tool in ip openssl tc iptables grpcurl; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            NETNS_MISSING_TOOL="$tool"
            return 1
        fi
    done
    ip netns list >/dev/null 2>&1 || return 1
    return 0
}

# netns_unsupported_reason — what to put in the skip message, so a skipped
# case says which tool is missing rather than just "unsupported".
netns_unsupported_reason() {
    if [[ "$(id -u)" -ne 0 ]]; then
        echo "needs root to create network namespaces"
    elif [[ -n "${NETNS_MISSING_TOOL:-}" ]]; then
        echo "needs $NETNS_MISSING_TOOL, which is not installed"
    else
        echo "network namespaces are unavailable on this host"
    fi
}

netns_name() { echo "${NETNS_PREFIX}${1}"; }
netns_ip() { echo "${NETNS_SUBNET}.${1}"; }
netns_peer_url() { echo "https://$(netns_ip "$1"):${NETNS_PEER_PORT}"; }
netns_client_url() { echo "https://$(netns_ip "$1"):${NETNS_CLIENT_PORT}"; }

# netns_pki <root> <count> — generate ONE CA per trust domain, shared by every
# member.
#
# This is the harness standing in for the operator. nodestore deliberately
# refuses to generate its own material for a clustered member (see
# crates/nodestore/src/tls.rs): each member would mint a CA only it trusted,
# so nothing would trust anything else and the cluster would never form. A
# real deployment supplies a common CA; so does this.
#
# One server certificate is shared by all members, with every member IP in its
# SANs — simpler than issuing one per member, and equivalent for the property
# under test.
netns_pki() {
    local root="$1" count="$2" dir="$root/pki"
    [[ -f "$dir/ca.crt" ]] && { echo "$dir"; return 0; }
    mkdir -p "$dir"

    local sans="IP:127.0.0.1,DNS:localhost" i
    for ((i = 1; i <= count; i++)); do
        sans="$sans,IP:$(netns_ip "$i")"
    done

    # basicConstraints/keyUsage stated explicitly rather than left to the
    # host's openssl.cnf: a CA certificate without CA:TRUE and keyCertSign is
    # rejected as an issuer by rustls, and the resulting failure is a
    # handshake error during cluster setup that looks like a network fault.
    # Which defaults apply varies by distro, so this cannot be left implicit.
    local domain
    for domain in ca peer-ca; do
        openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
            -keyout "$dir/$domain.key" -out "$dir/$domain.crt" \
            -subj "/CN=nodestore-test-$domain" \
            -addext "basicConstraints=critical,CA:TRUE" \
            -addext "keyUsage=critical,keyCertSign,cRLSign,digitalSignature" \
            >/dev/null 2>&1
    done

    # Leaf certs need both server and client usage: a member is a server to
    # its peers on one connection and a client to them on another, and a
    # follower forwarding a write is a client of the leader's client API.
    local leaf
    for leaf in server:ca peer:peer-ca client:ca; do
        local name="${leaf%%:*}" issuer="${leaf##*:}"
        openssl req -newkey rsa:2048 -nodes \
            -keyout "$dir/$name.key" -out "$dir/$name.csr" \
            -subj "/CN=nodestore-test-$name" >/dev/null 2>&1
        openssl x509 -req -in "$dir/$name.csr" \
            -CA "$dir/$issuer.crt" -CAkey "$dir/$issuer.key" -CAcreateserial \
            -out "$dir/$name.crt" -days 3650 \
            -extfile <(printf 'subjectAltName=%s\nextendedKeyUsage=serverAuth,clientAuth\n' "$sans") \
            >/dev/null 2>&1
    done
    echo "$dir"
}

# The grpcurl flags every call needs now that there is no plaintext mode.
#
# Two sets, because the two listeners are in different trust domains on
# purpose: a client certificate must not be usable to join the raft cluster.
# Using the wrong set here fails the handshake, which is the property working.
netns_tls_flags() {
    local dir="${NETNS_PKI_DIR:?netns_pki must run first}"
    echo "-cacert $dir/ca.crt -cert $dir/client.crt -key $dir/client.key"
}

netns_peer_tls_flags() {
    local dir="${NETNS_PKI_DIR:?netns_pki must run first}"
    echo "-cacert $dir/peer-ca.crt -cert $dir/peer.crt -key $dir/peer.key"
}

# netns_cluster_spec <count> — the NODESTORE_INITIAL_CLUSTER value.
netns_cluster_spec() {
    local count="$1" i spec=""
    for ((i = 1; i <= count; i++)); do
        [[ -n "$spec" ]] && spec+=","
        spec+="${i}=$(netns_peer_url "$i")"
    done
    echo "$spec"
}

# netns_setup <count> — bridge plus one namespace per member.
netns_setup() {
    local count="$1" i ns

    ip link add "$NETNS_BRIDGE" type bridge 2>/dev/null || true
    ip link set "$NETNS_BRIDGE" up
    # An address on the bridge lets the test harness itself — which lives in
    # the root namespace — reach every member's client port.
    ip addr add "$(netns_ip 254)/24" dev "$NETNS_BRIDGE" 2>/dev/null || true

    for ((i = 1; i <= count; i++)); do
        ns="$(netns_name "$i")"
        ip netns add "$ns" 2>/dev/null || true
        ip link add "v${NETNS_PREFIX}${i}" type veth peer name "b${NETNS_PREFIX}${i}" 2>/dev/null || true
        ip link set "b${NETNS_PREFIX}${i}" master "$NETNS_BRIDGE"
        ip link set "b${NETNS_PREFIX}${i}" up
        ip link set "v${NETNS_PREFIX}${i}" netns "$ns"
        ip netns exec "$ns" ip addr add "$(netns_ip "$i")/24" dev "v${NETNS_PREFIX}${i}" 2>/dev/null || true
        ip netns exec "$ns" ip link set "v${NETNS_PREFIX}${i}" up
        ip netns exec "$ns" ip link set lo up
    done
}

# netns_teardown <count> — remove everything, best effort.
netns_teardown() {
    local count="$1" i
    for ((i = 1; i <= count; i++)); do
        ip netns pids "$(netns_name "$i")" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$(netns_name "$i")" 2>/dev/null
        ip link del "b${NETNS_PREFIX}${i}" 2>/dev/null
    done
    ip link del "$NETNS_BRIDGE" 2>/dev/null
    return 0
}

# netns_start_member <index> <count> <binary> <data-root> — run one member.
netns_start_member() {
    local i="$1" count="$2" bin="$3" root="$4"
    local ns; ns="$(netns_name "$i")"
    mkdir -p "$root/$i"
    # Assigned in the *caller*, not inside netns_pki: that function's output is
    # read through command substitution, which runs it in a subshell, so an
    # export there would never reach this shell. The grpcurl helpers below read
    # this variable from test bodies that never see the cluster root.
    NETNS_PKI_DIR="$(netns_pki "$root" "$count")"
    export NETNS_PKI_DIR
    local pki="$NETNS_PKI_DIR"
    ip netns exec "$ns" env \
        NODESTORE_MEMBER_ID="$i" \
        NODESTORE_INITIAL_CLUSTER="$(netns_cluster_spec "$count")" \
        NODESTORE_LISTEN="0.0.0.0:${NETNS_CLIENT_PORT}" \
        NODESTORE_ADVERTISE_CLIENT_URL="$(netns_client_url "$i")" \
        NODESTORE_DATA_DIR="$root/$i/data" \
        NODESTORE_CERT_FILE="$pki/server.crt" \
        NODESTORE_KEY_FILE="$pki/server.key" \
        NODESTORE_TRUSTED_CA_FILE="$pki/ca.crt" \
        NODESTORE_PEER_CERT_FILE="$pki/peer.crt" \
        NODESTORE_PEER_KEY_FILE="$pki/peer.key" \
        NODESTORE_PEER_TRUSTED_CA_FILE="$pki/peer-ca.crt" \
        RUST_LOG="${NETNS_LOG:-info}" \
        "$bin" nodestore >"$root/$i/nodestore.log" 2>&1 &
    echo $!
}

# netns_kill_member <index> — SIGKILL, no cleanup, no graceful anything.
#
# Deliberately SIGKILL: a member that gets to flush and exit tidily is not the
# failure worth testing. The interesting case is the one where it stops
# between an fsync and an acknowledgement.
netns_kill_member() {
    ip netns pids "$(netns_name "$1")" 2>/dev/null | xargs -r kill -9 2>/dev/null
    return 0
}

# netns_add_latency <index> <ms> — delay everything leaving this member.
netns_add_latency() {
    local i="$1" ms="$2"
    ip netns exec "$(netns_name "$i")" \
        tc qdisc replace dev "v${NETNS_PREFIX}${i}" root netem delay "${ms}ms" 2>/dev/null
    return 0
}

netns_clear_latency() {
    local i="$1"
    ip netns exec "$(netns_name "$i")" \
        tc qdisc del dev "v${NETNS_PREFIX}${i}" root 2>/dev/null
    return 0
}

# netns_partition <index> — cut this member off without stopping it.
#
# The distinction that matters: a killed member knows it is gone, whereas a
# partitioned one keeps believing it is the leader. Testing only kills would
# miss every split-brain bug there is.
netns_partition() {
    local i="$1"
    ip netns exec "$(netns_name "$i")" iptables -A INPUT -p tcp --dport "$NETNS_PEER_PORT" -j DROP 2>/dev/null
    ip netns exec "$(netns_name "$i")" iptables -A OUTPUT -p tcp --dport "$NETNS_PEER_PORT" -j DROP 2>/dev/null
    return 0
}

netns_heal() {
    local i="$1"
    ip netns exec "$(netns_name "$i")" iptables -F 2>/dev/null
    return 0
}

# netns_status <index> — the peer Status RPC, as JSON.
netns_status() {
    grpcurl $(netns_peer_tls_flags) -max-time 5 \
        -import-path "$REPO_ROOT/crates/nodestore/proto" -proto peer.proto \
        -d '{}' "$(netns_ip "$1"):${NETNS_PEER_PORT}" \
        notk8s.nodestore.peer.v1.Peer/Status 2>/dev/null
}

# netns_leader <count> — the index of the member every reachable member agrees
# is the leader, or "" if they do not agree yet.
#
# Agreement, not just "someone says they lead": during an election two members
# can each briefly believe different things, and a test that accepted the
# first answer would pass on a split brain.
netns_leader() {
    local count="$1" i leader="" seen=""
    for ((i = 1; i <= count; i++)); do
        local status
        status="$(netns_status "$i")"
        [[ -n "$status" ]] || continue
        local id
        id="$(printf '%s' "$status" | python3 -c '
import json,sys
try:
    print(json.loads(sys.stdin.read()).get("leaderId", "0"))
except Exception:
    print("")
' 2>/dev/null)"
        [[ -n "$id" && "$id" != "0" ]] || return 0
        if [[ -z "$seen" ]]; then
            seen="$id"
        elif [[ "$seen" != "$id" ]]; then
            return 0  # members disagree; no settled leader
        fi
    done
    leader="$seen"
    echo "$leader"
}

# netns_wait_for_new_leader <count> <old-leader> <timeout-secs> — wait for the
# survivors to agree on a leader that is NOT the given one.
#
# Distinct from netns_wait_for_leader for a reason that cost a real test
# failure: immediately after a leader is killed, the survivors still name it,
# because they have not yet reached their election timeout. A wait that
# accepts the first agreed answer therefore returns the *dead* leader
# instantly — and a test built on it would report a successful failover having
# observed no failover at all.
netns_wait_for_new_leader() {
    local count="$1" old="$2" timeout="${3:-60}" waited=0 leader
    while [[ "$waited" -lt "$timeout" ]]; do
        leader="$(netns_leader "$count")"
        if [[ -n "$leader" && "$leader" != "$old" ]]; then
            echo "$leader"
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

# netns_wait_for_leader <count> <timeout-secs> — echo the agreed leader id.
netns_wait_for_leader() {
    local count="$1" timeout="${2:-30}" waited=0 leader
    while [[ "$waited" -lt "$timeout" ]]; do
        leader="$(netns_leader "$count")"
        if [[ -n "$leader" ]]; then
            echo "$leader"
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

# netns_put <index> <key> <value> — write through this member's client port.
netns_put() {
    grpcurl $(netns_tls_flags) -max-time 15 \
        -import-path "$REPO_ROOT/crates/nodestore/proto" -proto rpc.proto \
        -d "{\"key\":\"$(printf '%s' "$2" | base64 -w0)\",\"value\":\"$(printf '%s' "$3" | base64 -w0)\"}" \
        "$(netns_ip "$1"):${NETNS_CLIENT_PORT}" etcdserverpb.KV/Put 2>&1
}

# netns_get <index> <key> [serializable] — read it back.
netns_get() {
    local serializable="${3:-false}"
    grpcurl $(netns_tls_flags) -max-time 15 \
        -import-path "$REPO_ROOT/crates/nodestore/proto" -proto rpc.proto \
        -d "{\"key\":\"$(printf '%s' "$2" | base64 -w0)\",\"serializable\":$serializable}" \
        "$(netns_ip "$1"):${NETNS_CLIENT_PORT}" etcdserverpb.KV/Range 2>/dev/null \
        | python3 -c '
import base64, json, sys
try:
    doc = json.loads(sys.stdin.read())
except Exception:
    sys.exit(0)
for kv in doc.get("kvs", []):
    sys.stdout.write(base64.b64decode(kv["value"]).decode("utf-8", "replace"))
    break
'
}
