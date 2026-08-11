# lib/test/cases/datastore.sh — nodestore, the etcd v3 datastore.
#
# These drive the real gRPC API over the wire with grpcurl (installed by
# e2e-misc-prereqs.sh), against a nodestore started on a scratch port and a
# scratch database. Deliberately NOT against the cluster's own datastore:
# these tests write, delete and compact, and doing that to the store the
# running control plane is using would be a test that breaks the cluster it
# is testing on.
#
# The unit tests in crates/nodestore cover the semantics from inside. What
# these add is everything the unit tests cannot reach: that the protos
# actually compile into a server a real gRPC client can talk to, that field
# numbers and JSON field names line up, that a bidirectional watch stream
# delivers events to a separate process, and that the binary starts at all.
#
# grpcurl speaks JSON, and the etcd API is bytes — so keys and values cross
# the wire base64-encoded. That is not this suite being awkward; it is what
# any JSON client of etcd has to do, and getting it wrong is a good way to
# "pass" a test that never checked the value it thought it did.

NODESTORE_TEST_PORT="${NODESTORE_TEST_PORT:-23790}"
NODESTORE_TEST_ADDR="127.0.0.1:${NODESTORE_TEST_PORT}"

_nodestore_binary() {
    local candidate
    for candidate in "$REPO_ROOT/bin/nodestore" "$REPO_ROOT/target/release/nodestore" \
                     "$REPO_ROOT/target/debug/nodestore"; do
        [[ -x "$candidate" ]] && { echo "$candidate"; return 0; }
    done
    # The combined layout may hold it without a symlink if only nodelet and
    # nodeproxy were linked (DATASTORE unset at deploy time).
    if [[ -x "$REPO_ROOT/bin/notk8s" ]] && "$REPO_ROOT/bin/notk8s" components 2>/dev/null | grep -qx nodestore; then
        echo "$REPO_ROOT/bin/notk8s"
        return 0
    fi
    echo ""
}

# Start a throwaway nodestore. Sets $ns_pid, $ns_dir, $ns_log — plain
# (not `local`) because _nodestore_stop runs from an EXIT trap, which can
# fire after the calling function's locals are gone. Same hazard, and the
# same fix, as bootstrap.sh's own throwaway-nodelet test.
_nodestore_start() {
    local bin
    bin="$(_nodestore_binary)"
    [[ -n "$bin" ]] || skip_test "no nodestore binary (build with DATASTORE=nodestore, or --layout=combined)"
    command -v grpcurl >/dev/null 2>&1 || skip_test "needs grpcurl (deploy/lib/e2e-misc-prereqs.sh installs it)"

    ns_dir="$(mktemp -d)"
    ns_log="$ns_dir/nodestore.log"
    NODESTORE_LISTEN="$NODESTORE_TEST_ADDR" \
    NODESTORE_DATA_DIR="$ns_dir/data" \
    RUST_LOG="${NODESTORE_TEST_LOG:-info}" \
        "$bin" nodestore >"$ns_log" 2>&1 &
    # The trailing `nodestore` is the applet name for a combined binary; the
    # standalone one takes no arguments and ignores it, so one invocation
    # covers both layouts.
    ns_pid=$!

    # Wait for the port rather than sleeping: a fixed sleep is either flaky
    # or slow, and on a loaded CI runner usually both.
    local waited=0
    until _nodestore_rpc etcdserverpb.Maintenance/Status '{}' >/dev/null 2>&1; do
        if ! kill -0 "$ns_pid" 2>/dev/null; then
            die "nodestore exited during startup — log: $(cat "$ns_log")"
        fi
        waited=$((waited + 1))
        [[ "$waited" -gt 100 ]] && die "nodestore never answered on $NODESTORE_TEST_ADDR within 20s — log: $(cat "$ns_log")"
        sleep 0.2
    done
}

_nodestore_stop() {
    [[ -n "${ns_pid:-}" ]] && kill "$ns_pid" 2>/dev/null
    [[ -n "${ns_pid:-}" ]] && wait "$ns_pid" 2>/dev/null
    ns_pid=""
    [[ -n "${ns_dir:-}" ]] && rm -rf "$ns_dir"
}

# _nodestore_rpc <service/method> <json> — one unary call.
#
# -import-path/-proto rather than server reflection: nodestore does not serve
# the reflection service (one more thing on the wire for no runtime benefit),
# and the protos are right here in the repo.
_nodestore_rpc() {
    grpcurl $(_nodestore_tls_flags) -max-time 10 \
        -import-path "$REPO_ROOT/crates/nodestore/proto" -proto rpc.proto \
        -d "$2" "$NODESTORE_TEST_ADDR" "$1" 2>&1
}

# The client certificate nodestore generated for itself on first start.
#
# There is no plaintext mode — the store holds every Secret in a real cluster
# and the etcd API has no authentication of its own, so the listener always
# requires a client certificate. For a single member the material is generated
# into the data directory, which is exactly where these tests point it.
_nodestore_tls_flags() {
    local pki="${ns_dir:?_nodestore_start must run first}/data/pki/client"
    echo "-cacert $pki/ca.crt -cert $pki/client.crt -key $pki/client.key"
}

_b64() { printf '%s' "$1" | base64 -w0; }

# Range for a single key, returning the decoded value or "" if absent.
_nodestore_get() {
    local out
    out="$(_nodestore_rpc etcdserverpb.KV/Range "{\"key\":\"$(_b64 "$1")\"}")"
    # jq isn't guaranteed on these hosts; python3 is (the suite already uses
    # it elsewhere) and it decodes base64 without another dependency.
    printf '%s' "$out" | python3 -c '
import base64, json, sys
try:
    doc = json.loads(sys.stdin.read())
except json.JSONDecodeError:
    sys.exit(0)
for kv in doc.get("kvs", []):
    sys.stdout.write(base64.b64decode(kv["value"]).decode("utf-8", "replace"))
    break
'
}

test_datastore_serves_the_etcd_status_rpc() {
    _nodestore_start
    trap _nodestore_stop EXIT

    # The first thing any etcd client does, and what apiserver gates its
    # startup on: an etcd version it can parse and consider new enough.
    local out
    out="$(_nodestore_rpc etcdserverpb.Maintenance/Status '{}')"
    assert_contains "$out" '"version"' "Status should report a version"
    assert_contains "$out" '3.' "the reported version must look like an etcd version, not ours"

    _nodestore_stop
    trap - EXIT
}

test_datastore_round_trips_a_key_over_grpc() {
    _nodestore_start
    trap _nodestore_stop EXIT

    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/test/a)\",\"value\":\"$(_b64 hello)\"}" >/dev/null
    assert_eq "$(_nodestore_get /registry/test/a)" "hello" "value read back over gRPC"

    # Overwrite, then delete — the full lifecycle a client actually uses.
    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/test/a)\",\"value\":\"$(_b64 goodbye)\"}" >/dev/null
    assert_eq "$(_nodestore_get /registry/test/a)" "goodbye" "value after overwrite"

    _nodestore_rpc etcdserverpb.KV/DeleteRange "{\"key\":\"$(_b64 /registry/test/a)\"}" >/dev/null
    assert_eq "$(_nodestore_get /registry/test/a)" "" "key is gone after delete"

    _nodestore_stop
    trap - EXIT
}

test_datastore_lists_a_prefix_in_key_order() {
    _nodestore_start
    trap _nodestore_stop EXIT

    # Written out of order on purpose: the ordering must come from the store,
    # not from insertion order.
    for k in c a b; do
        _nodestore_rpc etcdserverpb.KV/Put \
            "{\"key\":\"$(_b64 "/registry/pods/$k")\",\"value\":\"$(_b64 "$k")\"}" >/dev/null
    done
    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/nodes/n)\",\"value\":\"$(_b64 n)\"}" >/dev/null

    # A prefix scan is spelled [prefix, prefix-with-last-byte-incremented) —
    # exactly what apiserver sends for every LIST it does.
    local out keys
    out="$(_nodestore_rpc etcdserverpb.KV/Range \
        "{\"key\":\"$(_b64 /registry/pods/)\",\"rangeEnd\":\"$(_b64 /registry/pods0)\"}")"
    keys="$(printf '%s' "$out" | python3 -c '
import base64, json, sys
doc = json.loads(sys.stdin.read())
print(",".join(base64.b64decode(kv["key"]).decode() for kv in doc.get("kvs", [])))
')"
    assert_eq "$keys" "/registry/pods/a,/registry/pods/b,/registry/pods/c" "prefix listed in key order"
    assert_not_contains "$keys" "/registry/nodes/" "the prefix must not leak into the next one"

    _nodestore_stop
    trap - EXIT
}

test_datastore_enforces_compare_and_swap() {
    _nodestore_start
    trap _nodestore_stop EXIT

    # Every write apiserver makes is this transaction. If a stale
    # modRevision can win it, two clients updating one object both succeed
    # and one silently overwrites the other.
    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/cas)\",\"value\":\"$(_b64 v1)\"}" >/dev/null
    local stale_rev
    stale_rev="$(_nodestore_rpc etcdserverpb.KV/Range "{\"key\":\"$(_b64 /registry/cas)\"}" \
        | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["kvs"][0]["modRevision"])')"
    assert_not_empty "$stale_rev" "should have read a modRevision to compare against"

    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/cas)\",\"value\":\"$(_b64 v2)\"}" >/dev/null

    local out
    out="$(_nodestore_rpc etcdserverpb.KV/Txn "{
        \"compare\":[{\"key\":\"$(_b64 /registry/cas)\",\"result\":\"EQUAL\",\"target\":\"MOD\",\"modRevision\":\"$stale_rev\"}],
        \"success\":[{\"requestPut\":{\"key\":\"$(_b64 /registry/cas)\",\"value\":\"$(_b64 v3)\"}}],
        \"failure\":[{\"requestRange\":{\"key\":\"$(_b64 /registry/cas)\"}}]
    }")"
    assert_not_contains "$out" '"succeeded": true' "a stale compare-and-swap must lose"
    assert_eq "$(_nodestore_get /registry/cas)" "v2" "the losing write must not have applied"

    _nodestore_stop
    trap - EXIT
}

test_datastore_creates_a_key_only_if_absent() {
    _nodestore_start
    trap _nodestore_stop EXIT

    # modRevision == 0 against a missing key is how every object creation
    # reaches the datastore. A store that treated "no such key" as "cannot
    # compare" would fail every create.
    local create="{
        \"compare\":[{\"key\":\"$(_b64 /registry/new)\",\"result\":\"EQUAL\",\"target\":\"MOD\",\"modRevision\":\"0\"}],
        \"success\":[{\"requestPut\":{\"key\":\"$(_b64 /registry/new)\",\"value\":\"$(_b64 first)\"}}],
        \"failure\":[]
    }"
    local out
    out="$(_nodestore_rpc etcdserverpb.KV/Txn "$create")"
    assert_contains "$out" '"succeeded": true' "creating an absent key should succeed"
    assert_eq "$(_nodestore_get /registry/new)" "first" "the created value"

    # The same request again must fail — the key now exists.
    out="$(_nodestore_rpc etcdserverpb.KV/Txn "$create")"
    assert_not_contains "$out" '"succeeded": true' "creating an existing key must fail"

    _nodestore_stop
    trap - EXIT
}

test_datastore_streams_watch_events_as_they_happen() {
    _nodestore_start
    trap _nodestore_stop EXIT

    # The whole reason this component exists: a watcher is told about a
    # change because the change happened, not because it asked again.
    local watch_out="$ns_dir/watch.json"
    grpcurl $(_nodestore_tls_flags) -max-time 15 \
        -import-path "$REPO_ROOT/crates/nodestore/proto" -proto rpc.proto \
        -d "{\"createRequest\":{\"key\":\"$(_b64 /registry/watched/)\",\"rangeEnd\":\"$(_b64 /registry/watched0)\"}}" \
        "$NODESTORE_TEST_ADDR" etcdserverpb.Watch/Watch >"$watch_out" 2>&1 &
    local watch_pid=$!

    # Give the stream time to be created before writing, otherwise this
    # tests nothing — a watch from revision 0 starts at "now".
    sleep 2
    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/watched/x)\",\"value\":\"$(_b64 seen)\"}" >/dev/null
    _nodestore_rpc etcdserverpb.KV/DeleteRange \
        "{\"key\":\"$(_b64 /registry/watched/x)\"}" >/dev/null
    # Written outside the watched range: it must not appear.
    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/other/y)\",\"value\":\"$(_b64 unseen)\"}" >/dev/null

    # try_wait_until, not sleep: the events arrive when they arrive.
    try_wait_until 15 bash -c "grep -q DELETE '$watch_out'" \
        || die "the watch stream never delivered a DELETE — got: $(cat "$watch_out")"

    kill "$watch_pid" 2>/dev/null
    wait "$watch_pid" 2>/dev/null

    local body
    body="$(cat "$watch_out")"
    assert_contains "$body" '"created": true' "the stream should confirm the watch was created"
    assert_contains "$body" "$(_b64 /registry/watched/x)" "the watched key should appear in an event"
    assert_contains "$body" "DELETE" "the deletion should arrive as a DELETE event"
    assert_not_contains "$body" "$(_b64 /registry/other/y)" "a key outside the range must not be delivered"

    _nodestore_stop
    trap - EXIT
}

test_datastore_replays_missed_events_to_a_late_watcher() {
    _nodestore_start
    trap _nodestore_stop EXIT

    # A watcher that reconnects asks to resume from where it got to. If
    # replay is broken, apiserver's watch cache silently misses everything
    # that happened while it was disconnected.
    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/replay/a)\",\"value\":\"$(_b64 one)\"}" >/dev/null
    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/replay/b)\",\"value\":\"$(_b64 two)\"}" >/dev/null

    local watch_out="$ns_dir/replay.json"
    # From revision 2 — the first write this store ever did.
    timeout 12 grpcurl $(_nodestore_tls_flags) -max-time 10 \
        -import-path "$REPO_ROOT/crates/nodestore/proto" -proto rpc.proto \
        -d "{\"createRequest\":{\"key\":\"$(_b64 /registry/replay/)\",\"rangeEnd\":\"$(_b64 /registry/replay0)\",\"startRevision\":\"2\"}}" \
        "$NODESTORE_TEST_ADDR" etcdserverpb.Watch/Watch >"$watch_out" 2>&1 || true

    local body
    body="$(cat "$watch_out")"
    assert_contains "$body" "$(_b64 /registry/replay/a)" "replay should include the first write"
    assert_contains "$body" "$(_b64 /registry/replay/b)" "replay should include the second write"

    _nodestore_stop
    trap - EXIT
}

test_datastore_refuses_a_read_below_the_compaction_point() {
    _nodestore_start
    trap _nodestore_stop EXIT

    # The error apiserver depends on: it is what makes the watch cache
    # re-list rather than continue from a revision whose history is gone.
    # A store that answered such a read from what survived would hand back a
    # quietly incorrect view of the past.
    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/c)\",\"value\":\"$(_b64 v1)\"}" >/dev/null
    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/c)\",\"value\":\"$(_b64 v2)\"}" >/dev/null

    local current
    current="$(_nodestore_rpc etcdserverpb.Maintenance/Status '{}' \
        | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["header"]["revision"])')"
    _nodestore_rpc etcdserverpb.KV/Compact "{\"revision\":\"$current\"}" >/dev/null

    local out
    out="$(_nodestore_rpc etcdserverpb.KV/Range "{\"key\":\"$(_b64 /registry/c)\",\"revision\":\"2\"}")"
    assert_contains "$out" "compacted" "reading below the compaction point must fail with etcd's own wording"
    # ...and the live key still reads fine at the current revision.
    assert_eq "$(_nodestore_get /registry/c)" "v2" "compaction must not disturb live keys"

    _nodestore_stop
    trap - EXIT
}

test_datastore_expires_a_lease_and_its_keys() {
    _nodestore_start
    trap _nodestore_stop EXIT

    # Leases are how apiserver gives Events a TTL. A lease that never
    # expires means an Events table that grows forever.
    local grant lease_id
    grant="$(_nodestore_rpc etcdserverpb.Lease/LeaseGrant '{"TTL":"1"}')"
    lease_id="$(printf '%s' "$grant" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["ID"])')"
    assert_not_empty "$lease_id" "LeaseGrant should return an id"

    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/leased)\",\"value\":\"$(_b64 temp)\",\"lease\":\"$lease_id\"}" >/dev/null
    assert_eq "$(_nodestore_get /registry/leased)" "temp" "the leased key exists while the lease does"

    local waited=0
    while [[ -n "$(_nodestore_get /registry/leased)" ]]; do
        waited=$((waited + 1))
        [[ "$waited" -gt 30 ]] && die "the leased key outlived its 1s lease by 30s — is the expiry loop running?"
        sleep 1
    done

    _nodestore_stop
    trap - EXIT
}

test_datastore_survives_a_restart_with_its_data() {
    _nodestore_start
    trap _nodestore_stop EXIT

    _nodestore_rpc etcdserverpb.KV/Put \
        "{\"key\":\"$(_b64 /registry/durable)\",\"value\":\"$(_b64 persisted)\"}" >/dev/null
    local revision_before
    revision_before="$(_nodestore_rpc etcdserverpb.Maintenance/Status '{}' \
        | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["header"]["revision"])')"

    # Restart against the same data directory. A control plane's datastore
    # that lost state on restart would be worse than no datastore.
    local bin keep_dir="$ns_dir"
    bin="$(_nodestore_binary)"
    kill "$ns_pid" 2>/dev/null
    wait "$ns_pid" 2>/dev/null
    NODESTORE_LISTEN="$NODESTORE_TEST_ADDR" NODESTORE_DATA_DIR="$keep_dir/data" \
        "$bin" nodestore >>"$ns_log" 2>&1 &
    ns_pid=$!
    local waited=0
    until _nodestore_rpc etcdserverpb.Maintenance/Status '{}' >/dev/null 2>&1; do
        waited=$((waited + 1))
        [[ "$waited" -gt 100 ]] && die "nodestore did not come back up after a restart — log: $(cat "$ns_log")"
        sleep 0.2
    done

    assert_eq "$(_nodestore_get /registry/durable)" "persisted" "data survives a restart"
    local revision_after
    revision_after="$(_nodestore_rpc etcdserverpb.Maintenance/Status '{}' \
        | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["header"]["revision"])')"
    assert_eq "$revision_after" "$revision_before" "the revision counter must not restart"

    _nodestore_stop
    trap - EXIT
}

test_datastore_refuses_a_cluster_it_cannot_be_part_of() {
    # This replaces a test that asserted NODESTORE_PEERS was *refused* because
    # replication was not implemented. It is now implemented, so that refusal
    # is gone and asserting it would be asserting a promise the code no longer
    # makes.
    #
    # What still has to hold is the other half of that honesty: a
    # misconfiguration must fail loudly at startup rather than producing a
    # member that runs and never joins anything. A member absent from its own
    # initial cluster campaigns for a cluster it is not a voter in and can
    # never win — which presents as "no leader is ever elected" rather than as
    # the typo it is.
    local bin
    bin="$(_nodestore_binary)"
    [[ -n "$bin" ]] || skip_test "no nodestore binary"

    local dir out rc=0
    dir="$(mktemp -d)"
    out="$(NODESTORE_LISTEN=127.0.0.1:23791 NODESTORE_DATA_DIR="$dir/data" \
        NODESTORE_MEMBER_ID=9 \
        NODESTORE_INITIAL_CLUSTER="1=http://10.0.0.1:2380,2=http://10.0.0.2:2380" \
        timeout 20 "$bin" nodestore 2>&1)" || rc=$?
    rm -rf "$dir"

    assert_not_eq "$rc" "0" "a member missing from its own cluster must refuse to start"
    assert_contains "$out" "does not appear in the initial cluster" "the refusal should say what is wrong"
}

test_datastore_refuses_a_malformed_cluster_spec() {
    local bin
    bin="$(_nodestore_binary)"
    [[ -n "$bin" ]] || skip_test "no nodestore binary"

    local dir out rc=0
    dir="$(mktemp -d)"
    # A peer URL with no scheme. Accepting it would fail later, at the point
    # a peer is dialled, by which time the member looks healthy.
    out="$(NODESTORE_LISTEN=127.0.0.1:23791 NODESTORE_DATA_DIR="$dir/data" \
        NODESTORE_INITIAL_CLUSTER="1=10.0.0.1:2380" \
        timeout 20 "$bin" nodestore 2>&1)" || rc=$?
    rm -rf "$dir"

    assert_not_eq "$rc" "0" "a schemeless peer URL must be refused at startup"
    assert_contains "$out" "must include a scheme" "the refusal should name the problem"
}

register_test test_datastore_serves_the_etcd_status_rpc
register_test test_datastore_round_trips_a_key_over_grpc
register_test test_datastore_lists_a_prefix_in_key_order
register_test test_datastore_enforces_compare_and_swap
register_test test_datastore_creates_a_key_only_if_absent
register_test test_datastore_streams_watch_events_as_they_happen
register_test test_datastore_replays_missed_events_to_a_late_watcher
register_test test_datastore_refuses_a_read_below_the_compaction_point
register_test test_datastore_expires_a_lease_and_its_keys
register_test test_datastore_survives_a_restart_with_its_data
register_test test_datastore_refuses_a_cluster_it_cannot_be_part_of
register_test test_datastore_refuses_a_malformed_cluster_spec
