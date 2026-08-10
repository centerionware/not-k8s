# lib/test/cases/datastore_cluster.sh — replication, elections and failover.
#
# A real three-member nodestore cluster: three processes, three network
# namespaces, three veths into a bridge. Everything crosses a real network
# stack, which is what lets these tests break it in the specific ways that
# matter (see deploy/lib/test/netns.sh on why namespaces rather than ports).
#
# What each of these is actually protecting against:
#
#   * **Replication** — a write to one member has to be readable from the
#     others. If it isn't, the cluster is three independent datastores.
#   * **Forwarding** — apiserver is configured with an endpoint list and
#     expects any of them to accept writes. A follower that refused would make
#     a three-member cluster serve writes a third of the time.
#   * **Leader death** — the case the whole design exists for. A new leader
#     must be elected and must still hold every committed write.
#   * **Follower death** — a majority must keep serving. A cluster that stops
#     when a minority dies has negative value over a single node.
#   * **Quorum loss** — with two of three gone the survivor must *refuse*
#     writes. Accepting them would be split-brain: the write is unrecoverable
#     and silently lost when the majority returns.
#   * **Rejoining** — a member that was dead must catch up, including from a
#     snapshot if the log moved past it while it was away.
#
# These take minutes, not seconds: elections are bounded below by the election
# timeout, and rushing them by shortening it would make the tests pass under
# conditions the real thing never runs in.

CLUSTER_SIZE=3
CLUSTER_ROOT=""

_cluster_binary() {
    local candidate
    for candidate in "$REPO_ROOT/bin/nodestore" "$REPO_ROOT/target/release/nodestore" \
                     "$REPO_ROOT/target/debug/nodestore"; do
        [[ -x "$candidate" ]] && { echo "$candidate"; return 0; }
    done
    if [[ -x "$REPO_ROOT/bin/notk8s" ]] && "$REPO_ROOT/bin/notk8s" components 2>/dev/null | grep -qx nodestore; then
        echo "$REPO_ROOT/bin/notk8s"
        return 0
    fi
    echo ""
}

# Bring up a whole cluster. Plain (not `local`) variables: read from the EXIT
# trap, which can fire after the calling function's locals are gone.
_cluster_start() {
    local bin
    bin="$(_cluster_binary)"
    [[ -n "$bin" ]] || skip_test "no nodestore binary (build with DATASTORE=nodestore)"
    command -v grpcurl >/dev/null 2>&1 || skip_test "needs grpcurl"
    netns_supported || skip_test "needs root and 'ip netns' for a real multi-node cluster"

    CLUSTER_ROOT="$(mktemp -d)"
    netns_teardown "$CLUSTER_SIZE"   # in case a previous run died mid-test
    netns_setup "$CLUSTER_SIZE"

    local i
    for ((i = 1; i <= CLUSTER_SIZE; i++)); do
        netns_start_member "$i" "$CLUSTER_SIZE" "$bin" "$CLUSTER_ROOT" >/dev/null
    done

    cluster_leader="$(netns_wait_for_leader "$CLUSTER_SIZE" 45)" \
        || die "the cluster never elected a leader within 45s — logs: $(tail -20 "$CLUSTER_ROOT"/*/nodestore.log 2>/dev/null)"
}

_cluster_stop() {
    [[ -n "${CLUSTER_ROOT:-}" ]] || return 0
    netns_teardown "$CLUSTER_SIZE"
    rm -rf "$CLUSTER_ROOT"
    CLUSTER_ROOT=""
}

# A member that is not the leader.
_a_follower() {
    local i
    for ((i = 1; i <= CLUSTER_SIZE; i++)); do
        [[ "$i" != "$1" ]] && { echo "$i"; return 0; }
    done
}

test_cluster_elects_a_single_leader() {
    _cluster_start
    trap _cluster_stop EXIT

    assert_not_empty "$cluster_leader" "the cluster should have elected a leader"
    # Every member must name the *same* leader. Two members each believing
    # they lead is the failure this whole component exists to prevent, and it
    # would otherwise show up only as data loss much later.
    local agreed
    agreed="$(netns_leader "$CLUSTER_SIZE")"
    assert_eq "$agreed" "$cluster_leader" "every member must agree on one leader"

    _cluster_stop
    trap - EXIT
}

test_cluster_replicates_a_write_to_every_member() {
    _cluster_start
    trap _cluster_stop EXIT

    netns_put "$cluster_leader" /registry/replicated hello >/dev/null

    # Read from each member with serializable=true so the read is answered
    # locally rather than forwarded — otherwise this would prove only that
    # forwarding works, not that the data actually got there.
    local i
    for ((i = 1; i <= CLUSTER_SIZE; i++)); do
        local got waited=0
        until [[ "$(netns_get "$i" /registry/replicated true)" == "hello" ]]; do
            waited=$((waited + 1))
            [[ "$waited" -gt 30 ]] \
                && die "member $i never received the replicated write — log: $(tail -20 "$CLUSTER_ROOT/$i/nodestore.log")"
            sleep 1
        done
        got="$(netns_get "$i" /registry/replicated true)"
        assert_eq "$got" "hello" "member $i holds the replicated value locally"
    done

    _cluster_stop
    trap - EXIT
}

test_a_follower_forwards_writes_to_the_leader() {
    _cluster_start
    trap _cluster_stop EXIT

    # apiserver is given an endpoint list and expects any of them to accept a
    # write. A follower that refused would make this cluster serve writes a
    # third of the time.
    local follower
    follower="$(_a_follower "$cluster_leader")"
    local out
    out="$(netns_put "$follower" /registry/forwarded viafollower)"
    assert_not_contains "$out" "not the leader" "a follower must forward, not refuse"

    local waited=0
    until [[ "$(netns_get "$cluster_leader" /registry/forwarded true)" == "viafollower" ]]; do
        waited=$((waited + 1))
        [[ "$waited" -gt 20 ]] && die "the forwarded write never reached the leader — $out"
        sleep 1
    done

    _cluster_stop
    trap - EXIT
}

test_the_cluster_survives_the_leader_being_killed() {
    _cluster_start
    trap _cluster_stop EXIT

    netns_put "$cluster_leader" /registry/before-failover survived >/dev/null
    sleep 2   # let it replicate to the followers that will outlive the leader

    local old_leader="$cluster_leader"
    # SIGKILL, not a graceful stop: a leader that gets to shut down tidily is
    # not the failure worth testing.
    netns_kill_member "$old_leader"

    local new_leader
    new_leader="$(netns_wait_for_leader "$CLUSTER_SIZE" 60)" \
        || die "no new leader was elected within 60s of killing member $old_leader"
    assert_not_eq "$new_leader" "$old_leader" "a dead member must not still be the leader"

    # The committed write must have survived the failover — this is the
    # promise raft exists to make.
    local survivor
    survivor="$(_a_follower "$old_leader")"
    assert_eq "$(netns_get "$survivor" /registry/before-failover true)" "survived" \
        "a write committed before the failover must survive it"

    # ...and the new leader must accept new writes.
    netns_put "$new_leader" /registry/after-failover ok >/dev/null
    local waited=0
    until [[ "$(netns_get "$new_leader" /registry/after-failover true)" == "ok" ]]; do
        waited=$((waited + 1))
        [[ "$waited" -gt 20 ]] && die "the new leader never accepted a write"
        sleep 1
    done

    _cluster_stop
    trap - EXIT
}

test_the_cluster_keeps_serving_when_a_follower_dies() {
    _cluster_start
    trap _cluster_stop EXIT

    # Two of three is a majority. A cluster that stopped here would have
    # negative value over a single node: more machines, less availability.
    local victim
    victim="$(_a_follower "$cluster_leader")"
    netns_kill_member "$victim"
    sleep 3

    local out
    out="$(netns_put "$cluster_leader" /registry/majority still-writable)"
    assert_not_contains "$out" "timed out" "a majority must keep accepting writes"
    assert_eq "$(netns_get "$cluster_leader" /registry/majority true)" "still-writable" \
        "the write should have committed on the surviving majority"

    _cluster_stop
    trap - EXIT
}

test_a_minority_refuses_writes_rather_than_inventing_them() {
    _cluster_start
    trap _cluster_stop EXIT

    # The most important negative result here. With two of three gone the
    # survivor cannot reach quorum, and accepting a write would be split
    # brain: unrecoverable, and silently discarded when the majority returns.
    local a b
    a="$(_a_follower "$cluster_leader")"
    b=""
    local i
    for ((i = 1; i <= CLUSTER_SIZE; i++)); do
        [[ "$i" != "$cluster_leader" && "$i" != "$a" ]] && b="$i"
    done
    netns_kill_member "$a"
    netns_kill_member "$b"
    sleep 5

    local out
    out="$(netns_put "$cluster_leader" /registry/should-not-commit nope)"
    # Either an explicit failure or a timeout is correct; silent success is
    # not.
    assert_not_contains "$out" '"header"' \
        "a member without quorum must not report a write as committed — got: $out"

    _cluster_stop
    trap - EXIT
}

test_a_restarted_member_catches_up_on_what_it_missed() {
    _cluster_start
    trap _cluster_stop EXIT

    local bin victim
    bin="$(_cluster_binary)"
    victim="$(_a_follower "$cluster_leader")"

    netns_kill_member "$victim"
    sleep 2

    # Write while it is away, so catching up is a real requirement rather
    # than a no-op.
    local n
    for n in 1 2 3 4 5; do
        netns_put "$cluster_leader" "/registry/missed-$n" "value-$n" >/dev/null
    done

    netns_start_member "$victim" "$CLUSTER_SIZE" "$bin" "$CLUSTER_ROOT" >/dev/null

    local waited=0
    until [[ "$(netns_get "$victim" /registry/missed-5 true)" == "value-5" ]]; do
        waited=$((waited + 1))
        [[ "$waited" -gt 45 ]] \
            && die "the restarted member never caught up — log: $(tail -30 "$CLUSTER_ROOT/$victim/nodestore.log")"
        sleep 1
    done
    assert_eq "$(netns_get "$victim" /registry/missed-1 true)" "value-1" \
        "it must catch up on everything it missed, not just the newest"

    _cluster_stop
    trap - EXIT
}

test_the_cluster_tolerates_a_slow_link() {
    _cluster_start
    trap _cluster_stop EXIT

    # tc lives in iproute2's tc package, which a minimal image may not have.
    # Skipping is the honest answer — without it, netem is a no-op and this
    # test would "pass" having injected no latency at all.
    command -v tc >/dev/null 2>&1 || skip_test "needs tc (iproute2) to inject latency"

    # 80ms each way is far beyond a datacentre and ordinary for a device on
    # the other end of a home connection — exactly the environment this
    # project targets. It must slow the cluster down, not depose its leader:
    # a link that re-elects under latency is one that stops serving writes
    # under load.
    local follower
    follower="$(_a_follower "$cluster_leader")"
    netns_add_latency "$follower" 80

    local before="$cluster_leader"
    netns_put "$cluster_leader" /registry/laggy ok >/dev/null
    sleep 5

    local after
    after="$(netns_leader "$CLUSTER_SIZE")"
    assert_eq "$after" "$before" "latency on one follower must not cause a leadership change"
    assert_eq "$(netns_get "$cluster_leader" /registry/laggy true)" "ok" \
        "writes must still commit over a slow link"

    netns_clear_latency "$follower"
    _cluster_stop
    trap - EXIT
}

test_a_partitioned_leader_steps_down_and_the_majority_elects_another() {
    _cluster_start
    trap _cluster_stop EXIT

    # The split-brain case, and the reason these tests use namespaces at all.
    # A *killed* leader knows it is gone; a *partitioned* one keeps believing
    # it leads. The majority must elect someone else, and the isolated member
    # must not still be accepting writes.
    local old_leader="$cluster_leader"
    netns_partition "$old_leader"

    local new_leader waited=0
    while [[ "$waited" -lt 60 ]]; do
        new_leader="$(netns_leader "$CLUSTER_SIZE")"
        [[ -n "$new_leader" && "$new_leader" != "$old_leader" ]] && break
        sleep 1
        waited=$((waited + 1))
    done
    [[ -n "$new_leader" && "$new_leader" != "$old_leader" ]] \
        || die "the majority never elected a new leader after partitioning member $old_leader"

    # The isolated member must have stopped claiming to lead: pre-vote means
    # it cannot win an election on its own, and without quorum it cannot
    # commit anything.
    local isolated_out
    isolated_out="$(netns_put "$old_leader" /registry/split-brain nope)"
    assert_not_contains "$isolated_out" '"header"' \
        "a partitioned member must not commit writes — got: $isolated_out"

    netns_heal "$old_leader"
    _cluster_stop
    trap - EXIT
}

register_test test_cluster_elects_a_single_leader
register_test test_cluster_replicates_a_write_to_every_member
register_test test_a_follower_forwards_writes_to_the_leader
register_test test_the_cluster_keeps_serving_when_a_follower_dies
register_test test_the_cluster_survives_the_leader_being_killed
register_test test_a_minority_refuses_writes_rather_than_inventing_them
register_test test_a_restarted_member_catches_up_on_what_it_missed
register_test test_the_cluster_tolerates_a_slow_link
register_test test_a_partitioned_leader_steps_down_and_the_majority_elects_another
