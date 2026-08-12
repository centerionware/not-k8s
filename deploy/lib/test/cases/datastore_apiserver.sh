# lib/test/cases/datastore_apiserver.sh — a real kube-apiserver, storing
# into nodestore.
#
# This is the test that decides whether nodestore is actually a datastore.
# Everything in datastore.sh checks that it answers the etcd API the way the
# spec says; this checks the only thing that matters, which is whether the
# one client that counts can run on it. kube-apiserver does not negotiate,
# does not degrade, and uses corners of the API (compare-and-swap on every
# write, prefix lists with paging, watches with prev_kv, leases for Events)
# that a hand-written test can assert individually and still collectively get
# wrong.
#
# It's the same method as deploy/lib/e2e-full-setup.sh fetching the real
# upstream CSI and DRA drivers rather than a hand-reconstructed copy: stand
# up the real implementation and watch it run. See docs/E2E_FINDINGS.md for
# what that has caught before.
#
# Why the standalone kube-apiserver binary and not a second k3s: two k3s
# servers on one host collide on the internal apiserver port (6444), which
# is not configurable, and stopping the running one would break every other
# test in the suite. The upstream binary takes --etcd-servers and nothing
# else this needs.

APISERVER_TEST_PORT="${APISERVER_TEST_PORT:-7443}"
NODESTORE_APISERVER_PORT="${NODESTORE_APISERVER_PORT:-23792}"

# Pinned to the k8s-openapi schema this project targets (see CLAUDE.md's note
# on why that pin exists), so the apiserver under test is the version whose
# storage behaviour the rest of the codebase assumes.
KUBE_APISERVER_VERSION="${KUBE_APISERVER_VERSION:-v1.33.0}"
# Only ever presented to a throwaway apiserver on loopback that exists for the
# length of one test.
APISERVER_TEST_TOKEN="${APISERVER_TEST_TOKEN:-nodestore-e2e-token}"

# The client API is mutual-TLS only — there is no plaintext mode. These are the
# paths nodestore generates for a single member on first start, and they are the
# same three files handed to kube-apiserver below.
_apisrv_tls_flags() {
    echo "-cacert $apisrv_dir/data/pki/client/ca.crt -cert $apisrv_dir/data/pki/client/client.crt -key $apisrv_dir/data/pki/client/client.key"
}

_apiserver_arch() {
    case "$(uname -m)" in
        x86_64) echo amd64 ;;
        aarch64) echo arm64 ;;
        armv7l) echo arm ;;
        *) echo "" ;;
    esac
}

# Fetch kube-apiserver once and cache it.
#
# NOT under $REPO_ROOT/.bootstrap, which is the obvious place and the wrong
# one: bootstrap-source.sh runs under sudo, so that directory is root-owned,
# and this suite runs unprivileged. Found live in CI, where the download
# failed with a permission error that this function then reported as "no
# network, or unsupported arch" — the misleading message cost more than the
# bug did. The cache lives somewhere the test user can actually write, and
# failures now say what actually happened.
_fetch_kube_apiserver() {
    local arch cache cache_dir err
    arch="$(_apiserver_arch)"
    if [[ -z "$arch" ]]; then
        _apiserver_fetch_error="unsupported architecture $(uname -m)"
        echo ""
        return 0
    fi

    cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/notk8s"
    mkdir -p "$cache_dir" 2>/dev/null || cache_dir="$(mktemp -d)"
    cache="$cache_dir/kube-apiserver-$KUBE_APISERVER_VERSION-$arch"
    if [[ -x "$cache" ]]; then
        echo "$cache"
        return 0
    fi

    # dl.k8s.io redirects to the release bucket, so -L is required, and the
    # canonical path includes /release/.
    if err="$(curl -fsSL --max-time 300 \
        "https://dl.k8s.io/release/${KUBE_APISERVER_VERSION}/bin/linux/${arch}/kube-apiserver" \
        -o "$cache.partial" 2>&1)"; then
        chmod +x "$cache.partial"
        mv "$cache.partial" "$cache"
        echo "$cache"
    else
        rm -f "$cache.partial"
        _apiserver_fetch_error="${err:-curl failed with no output}"
        echo ""
    fi
}

# Kill anything left over from a previous run of this test. Matched on the
# exact ports and binaries this test uses, so it cannot touch the cluster's
# own k3s or a nodestore some other test is running.
_apiserver_kill_strays() {
    local pids
    pids="$(pgrep -f "kube-apiserver-v.*--secure-port=$APISERVER_TEST_PORT" 2>/dev/null || true)"
    [[ -n "$pids" ]] && kill -9 $pids 2>/dev/null
    # The throwaway nodestore is identified by its port, which is this test's
    # own and not the one a deployed datastore would use.
    pids="$(pgrep -af "nodestore" 2>/dev/null | awk '{print $1}' || true)"
    local pid
    for pid in $pids; do
        if tr '\0' ' ' <"/proc/$pid/environ" 2>/dev/null | grep -q "NODESTORE_LISTEN=127.0.0.1:$NODESTORE_APISERVER_PORT"; then
            kill -9 "$pid" 2>/dev/null
        fi
    done
    return 0
}

# Both processes and the scratch dir. Not `local` — read from the EXIT trap,
# same reasoning as bootstrap.sh's own throwaway process test.
_apiserver_env_start() {
    local ns_bin api_bin
    ns_bin="$(_nodestore_binary)"
    [[ -n "$ns_bin" ]] || skip_test "no nodestore binary (build with DATASTORE=nodestore)"
    command -v openssl >/dev/null 2>&1 || skip_test "needs openssl to mint the service-account keypair"
    command -v kubectl >/dev/null 2>&1 || skip_test "needs kubectl"

    _apiserver_fetch_error=""
    api_bin="$(_fetch_kube_apiserver)"
    [[ -n "$api_bin" ]] \
        || skip_test "couldn't fetch kube-apiserver $KUBE_APISERVER_VERSION: ${_apiserver_fetch_error:-unknown reason}"

    # A previous run that was killed rather than exiting cleanly leaves both
    # processes alive — its cleanup is an EXIT trap, and a SIGKILL takes the
    # trap with it. Without this, the next run fails with "address already in
    # use", which says nothing about the datastore and sends you looking in
    # the wrong place. The cluster tests already do the equivalent with
    # netns_teardown.
    _apiserver_kill_strays

    apisrv_dir="$(mktemp -d)"
    apisrv_kubeconfig="$apisrv_dir/kubeconfig"
    ns_log="$apisrv_dir/nodestore.log"
    apisrv_log="$apisrv_dir/apiserver.log"

    # The datastore first — the apiserver will not start without it.
    NODESTORE_LISTEN="127.0.0.1:$NODESTORE_APISERVER_PORT" \
    NODESTORE_DATA_DIR="$apisrv_dir/data" \
        "$ns_bin" nodestore >"$ns_log" 2>&1 &
    ns_pid=$!

    # Mutual TLS rather than -plaintext: there is no unencrypted mode, so the
    # old probe could never succeed and this loop burned its whole budget
    # before reporting "never came up" about a store that had come up fine.
    # The material is generated by the store itself during the very startup
    # being waited on, so its absence means "not ready yet", not an error.
    local waited=0
    until [[ -s "$apisrv_dir/data/pki/client/client.key" ]] \
          && grpcurl $(_apisrv_tls_flags) -max-time 5 \
            -import-path "$REPO_ROOT/crates/nodestore/proto" -proto rpc.proto \
            -d '{}' "127.0.0.1:$NODESTORE_APISERVER_PORT" etcdserverpb.Maintenance/Status \
            >/dev/null 2>&1; do
        kill -0 "$ns_pid" 2>/dev/null || die "nodestore exited during startup — log: $(cat "$ns_log")"
        waited=$((waited + 1))
        [[ "$waited" -gt 100 ]] && die "nodestore never came up — log: $(cat "$ns_log")"
        sleep 0.2
    done

    # kube-apiserver insists on a service-account keypair even with
    # authentication effectively off.
    openssl genrsa -out "$apisrv_dir/sa.key" 2048 >/dev/null 2>&1
    openssl rsa -in "$apisrv_dir/sa.key" -pubout -out "$apisrv_dir/sa.pub" >/dev/null 2>&1

    # A static bearer token, not anonymous access.
    #
    # `--anonymous-auth=true` alongside `--authorization-mode=AlwaysAllow` is
    # silently overridden by kube-apiserver — it logs "AnonymousAuth is not
    # allowed with the AlwaysAllow authorizer. Resetting AnonymousAuth to
    # false" and carries on. A credential-less kubeconfig then cannot
    # authenticate, and every request hangs waiting for a username, which
    # presents as "the apiserver never became ready" and looks like a
    # datastore problem. It is not.
    #
    # A token file authenticates; AlwaysAllow authorizes. Authn/authz are not
    # what is under test here — the storage layer is — so the goal is for them
    # to be uninteresting rather than absent.
    echo "$APISERVER_TEST_TOKEN,e2e-admin,e2e-admin,system:masters" >"$apisrv_dir/tokens.csv"

    "$api_bin" \
        --etcd-servers="https://127.0.0.1:$NODESTORE_APISERVER_PORT" \
        --etcd-cafile="$apisrv_dir/data/pki/client/ca.crt" \
        --etcd-certfile="$apisrv_dir/data/pki/client/client.crt" \
        --etcd-keyfile="$apisrv_dir/data/pki/client/client.key" \
        --secure-port="$APISERVER_TEST_PORT" \
        --cert-dir="$apisrv_dir/certs" \
        --token-auth-file="$apisrv_dir/tokens.csv" \
        --service-account-key-file="$apisrv_dir/sa.pub" \
        --service-account-signing-key-file="$apisrv_dir/sa.key" \
        --service-account-issuer=https://kubernetes.default.svc \
        --service-cluster-ip-range=10.144.0.0/16 \
        --authorization-mode=AlwaysAllow \
        --allow-privileged=true \
        >"$apisrv_log" 2>&1 &
    apisrv_pid=$!

    cat >"$apisrv_kubeconfig" <<KUBECONFIG
apiVersion: v1
kind: Config
clusters:
- cluster:
    server: https://127.0.0.1:$APISERVER_TEST_PORT
    insecure-skip-tls-verify: true
  name: nodestore-test
contexts:
- context: {cluster: nodestore-test, user: e2e-admin}
  name: nodestore-test
current-context: nodestore-test
users:
- name: e2e-admin
  user:
    token: $APISERVER_TEST_TOKEN
KUBECONFIG

    # Real kube-apiserver startup is slow — it creates its own bootstrap
    # objects, the default namespaces, and the kubernetes service, all of
    # which are writes into nodestore.
    waited=0
    until kubectl --kubeconfig "$apisrv_kubeconfig" get --raw /readyz >/dev/null 2>&1; do
        if ! kill -0 "$apisrv_pid" 2>/dev/null; then
            die "kube-apiserver exited during startup against nodestore. This is the failure this whole test exists to catch — apiserver log tail: $(tail -40 "$apisrv_log"); nodestore log tail: $(tail -20 "$ns_log")"
        fi
        waited=$((waited + 1))
        [[ "$waited" -gt 180 ]] \
            && die "kube-apiserver never became ready against nodestore within 90s — apiserver log tail: $(tail -40 "$apisrv_log"); nodestore log tail: $(tail -20 "$ns_log")"
        sleep 0.5
    done
}

_apiserver_env_stop() {
    [[ -n "${apisrv_pid:-}" ]] && kill "$apisrv_pid" 2>/dev/null
    [[ -n "${apisrv_pid:-}" ]] && wait "$apisrv_pid" 2>/dev/null
    apisrv_pid=""
    [[ -n "${ns_pid:-}" ]] && kill "$ns_pid" 2>/dev/null
    [[ -n "${ns_pid:-}" ]] && wait "$ns_pid" 2>/dev/null
    ns_pid=""
    [[ -n "${apisrv_dir:-}" ]] && rm -rf "$apisrv_dir"
}

_kubectl_test() { kubectl --kubeconfig "$apisrv_kubeconfig" "$@"; }

test_a_real_apiserver_starts_and_serves_against_nodestore() {
    _apiserver_env_start
    trap _apiserver_env_stop EXIT

    # Getting here at all is most of the result: a real kube-apiserver
    # completed its own bootstrap — writing its default namespaces, the
    # kubernetes service, and its stored-version records — with nodestore as
    # its only storage.
    assert_contains "$(_kubectl_test get --raw /readyz)" "ok" "apiserver readyz"

    local namespaces
    namespaces="$(_kubectl_test get namespaces -o name 2>&1)"
    assert_contains "$namespaces" "namespace/default" "apiserver should have created the default namespace"
    assert_contains "$namespaces" "namespace/kube-system" "apiserver should have created kube-system"

    # ...and the data is genuinely in our store, not somewhere else.
    local keys
    keys="$(grpcurl $(_apisrv_tls_flags) -max-time 10 \
        -import-path "$REPO_ROOT/crates/nodestore/proto" -proto rpc.proto \
        -d "{\"key\":\"$(printf '%s' /registry/ | base64 -w0)\",\"rangeEnd\":\"$(printf '%s' /registry0 | base64 -w0)\",\"countOnly\":true}" \
        "127.0.0.1:$NODESTORE_APISERVER_PORT" etcdserverpb.KV/Range 2>&1 \
        | python3 -c 'import json,sys; print(json.loads(sys.stdin.read()).get("count","0"))')"
    [[ "${keys:-0}" -gt 10 ]] \
        || die "nodestore holds only ${keys:-0} keys under /registry/ — the apiserver is ready but its state went somewhere else"

    _apiserver_env_stop
    trap - EXIT
}

test_apiserver_crud_round_trips_through_nodestore() {
    _apiserver_env_start
    trap _apiserver_env_stop EXIT

    # Create → read → update → delete, through the whole real stack. The
    # update is the interesting one: apiserver guards it with a
    # compare-and-swap on the resourceVersion it read, so a store that got
    # modRevision wrong fails here and nowhere else.
    _kubectl_test create namespace nodestore-crud >/dev/null
    _kubectl_test -n nodestore-crud create configmap probe --from-literal=k=v1 >/dev/null
    assert_eq "$(_kubectl_test -n nodestore-crud get configmap probe -o jsonpath='{.data.k}')" "v1" "value after create"

    local rv1 rv2
    rv1="$(_kubectl_test -n nodestore-crud get configmap probe -o jsonpath='{.metadata.resourceVersion}')"
    _kubectl_test -n nodestore-crud create configmap probe --from-literal=k=v2 --dry-run=client -o yaml \
        | _kubectl_test -n nodestore-crud apply -f - >/dev/null
    assert_eq "$(_kubectl_test -n nodestore-crud get configmap probe -o jsonpath='{.data.k}')" "v2" "value after update"

    rv2="$(_kubectl_test -n nodestore-crud get configmap probe -o jsonpath='{.metadata.resourceVersion}')"
    assert_not_eq "$rv1" "$rv2" "an update must advance the resourceVersion — that is nodestore's revision surfacing"

    _kubectl_test -n nodestore-crud delete configmap probe >/dev/null
    assert_not_contains "$(_kubectl_test -n nodestore-crud get configmaps -o name 2>&1)" "configmap/probe" "gone after delete"

    _kubectl_test delete namespace nodestore-crud --wait=false >/dev/null 2>&1 || true
    _apiserver_env_stop
    trap - EXIT
}

test_apiserver_watch_delivers_through_nodestore() {
    _apiserver_env_start
    trap _apiserver_env_stop EXIT

    # `kubectl get --watch` is a real apiserver watch, which is a real
    # nodestore watch underneath — including the DELETE event, whose payload
    # apiserver builds out of prev_kv. A store that dropped prev_kv would
    # deliver a deletion of an object with no name, and this is where that
    # shows up.
    _kubectl_test create namespace nodestore-watch >/dev/null

    # -o json, not -o name: `-o name` prints only `kind/name` and drops the
    # event type entirely, so a test grepping it for a deletion can never
    # match no matter how correct the datastore is. Found by this test failing
    # while the deletion was in fact being delivered perfectly.
    local watch_out="$apisrv_dir/watch.json"
    timeout 30 kubectl --kubeconfig "$apisrv_kubeconfig" -n nodestore-watch \
        get configmaps --watch --output-watch-events -o json >"$watch_out" 2>&1 &
    local watch_pid=$!
    sleep 3

    _kubectl_test -n nodestore-watch create configmap watched --from-literal=k=v >/dev/null
    _kubectl_test -n nodestore-watch delete configmap watched >/dev/null

    try_wait_until 25 bash -c "grep -q DELETED '$watch_out'" \
        || die "the apiserver watch never reported the deletion — got: $(cat "$watch_out")"
    kill "$watch_pid" 2>/dev/null
    wait "$watch_pid" 2>/dev/null

    local body
    body="$(cat "$watch_out")"
    assert_contains "$body" "ADDED" "the creation should appear in the watch stream"
    # The deleted object arrives with its identity intact, which is what
    # apiserver builds out of nodestore's prev_kv. A DELETE carrying an empty
    # object would still show the type but not the name.
    assert_contains "$body" "watched" "the delete event must carry the object, not an empty shell"

    _kubectl_test delete namespace nodestore-watch --wait=false >/dev/null 2>&1 || true
    _apiserver_env_stop
    trap - EXIT
}

test_apiserver_state_survives_a_datastore_restart() {
    _apiserver_env_start
    trap _apiserver_env_stop EXIT

    _kubectl_test create namespace nodestore-durable >/dev/null
    _kubectl_test -n nodestore-durable create configmap kept --from-literal=k=v >/dev/null

    # Restart only the datastore, keeping its data directory, then bring a
    # fresh apiserver up against it. A control plane whose store forgets on
    # restart is not a control plane.
    local ns_bin api_bin
    ns_bin="$(_nodestore_binary)"
    api_bin="$(_fetch_kube_apiserver)"

    kill "$apisrv_pid" 2>/dev/null; wait "$apisrv_pid" 2>/dev/null; apisrv_pid=""
    kill "$ns_pid" 2>/dev/null; wait "$ns_pid" 2>/dev/null; ns_pid=""

    NODESTORE_LISTEN="127.0.0.1:$NODESTORE_APISERVER_PORT" \
    NODESTORE_DATA_DIR="$apisrv_dir/data" \
        "$ns_bin" nodestore >>"$ns_log" 2>&1 &
    ns_pid=$!
    "$api_bin" \
        --etcd-servers="https://127.0.0.1:$NODESTORE_APISERVER_PORT" \
        --etcd-cafile="$apisrv_dir/data/pki/client/ca.crt" \
        --etcd-certfile="$apisrv_dir/data/pki/client/client.crt" \
        --etcd-keyfile="$apisrv_dir/data/pki/client/client.key" \
        --secure-port="$APISERVER_TEST_PORT" \
        --cert-dir="$apisrv_dir/certs" \
        --token-auth-file="$apisrv_dir/tokens.csv" \
        --service-account-key-file="$apisrv_dir/sa.pub" \
        --service-account-signing-key-file="$apisrv_dir/sa.key" \
        --service-account-issuer=https://kubernetes.default.svc \
        --service-cluster-ip-range=10.144.0.0/16 \
        --authorization-mode=AlwaysAllow \
        >>"$apisrv_log" 2>&1 &
    apisrv_pid=$!

    local waited=0
    until _kubectl_test get --raw /readyz >/dev/null 2>&1; do
        kill -0 "$apisrv_pid" 2>/dev/null \
            || die "kube-apiserver did not come back up against the restarted datastore — log tail: $(tail -40 "$apisrv_log")"
        waited=$((waited + 1))
        [[ "$waited" -gt 180 ]] && die "apiserver never became ready after the datastore restart"
        sleep 0.5
    done

    assert_eq "$(_kubectl_test -n nodestore-durable get configmap kept -o jsonpath='{.data.k}')" "v" \
        "the object written before the restart is still there"

    _kubectl_test delete namespace nodestore-durable --wait=false >/dev/null 2>&1 || true
    _apiserver_env_stop
    trap - EXIT
}

register_test test_a_real_apiserver_starts_and_serves_against_nodestore
register_test test_apiserver_crud_round_trips_through_nodestore
register_test test_apiserver_watch_delivers_through_nodestore
register_test test_apiserver_state_survives_a_datastore_restart
