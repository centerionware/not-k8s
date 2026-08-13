# lib/test/cases/datastore_upgrade.sh — growing a running single member into
# a cluster without losing what it holds.
#
# This is the path a real deployment actually takes. Nobody starts at three
# members: they run one, it accumulates the entire cluster's state, and only
# then do they want redundancy. If that transition requires deleting the data
# directory, it is not an upgrade — the data directory *is* the cluster.
#
# It is also the transition with the most ways to quietly destroy everything,
# because single-member mode is not raft at all: `SingleNode` applies commands
# directly and writes no raft log, so a member that has been serving for months
# has a full state machine and literally no log behind it. Three separate bugs
# lived here, each found by running exactly this sequence on real hosts:
#
#   1. the conversion was refused outright, and the error told the operator to
#      delete the data;
#   2. doing what that error said panicked the member it was done to;
#   3. the panic left the process up and the service unit "active", so nothing
#      restarted it and every request failed for as long as anyone looked.
#
# So each test below asserts on the specific thing that went wrong, not just
# "the cluster works afterwards".

UPGRADE_ROOT=""
UPGRADE_SIZE=3

_upgrade_binary() {
    local candidate
    for candidate in "$REPO_ROOT/bin/nodestore" "$REPO_ROOT/target/release/nodestore" \
                     "$REPO_ROOT/target/debug/nodestore"; do
        [[ -x "$candidate" ]] && { echo "$candidate"; return 0; }
    done
    if [[ -x "$REPO_ROOT/bin/notk8s" ]] && "$REPO_ROOT/bin/notk8s" components 2>/dev/null | grep -qx nodestore; then
        echo "$REPO_ROOT/bin/notk8s"; return 0
    fi
    echo ""
}

_upgrade_prepare() {
    UPGRADE_BIN="$(_upgrade_binary)"
    [[ -n "$UPGRADE_BIN" ]] || skip_test "no nodestore binary (build with DATASTORE=nodestore)"
    netns_supported || skip_test "$(netns_unsupported_reason) — this needs real network namespaces"
    UPGRADE_ROOT="$(mktemp -d)"
    netns_teardown "$UPGRADE_SIZE"
    netns_setup "$UPGRADE_SIZE"
}

_upgrade_stop() {
    [[ -n "${UPGRADE_ROOT:-}" ]] || return 0
    netns_teardown "$UPGRADE_SIZE"
    rm -rf "$UPGRADE_ROOT"
    UPGRADE_ROOT=""
}

# Start member 1 the way a real deployment starts: one member, no cluster
# configured at all, so no raft and no log.
_start_as_single_member() {
    NETNS_CLUSTER_SPEC="" netns_start_member 1 "$UPGRADE_SIZE" "$UPGRADE_BIN" "$UPGRADE_ROOT" >/dev/null
}

# Restart member 1 as a one-member *cluster*, over the data it already has.
_restart_as_one_member_cluster() {
    NETNS_CLUSTER_SPEC="1=$(netns_peer_url 1)" \
        netns_start_member 1 "$UPGRADE_SIZE" "$UPGRADE_BIN" "$UPGRADE_ROOT" >/dev/null
}

_member1_log() { tail -40 "$UPGRADE_ROOT/1/nodestore.log" 2>/dev/null; }

test_upgrade_a_populated_single_member_into_a_one_member_cluster() {
    _upgrade_prepare
    trap _upgrade_stop EXIT

    _start_as_single_member
    # A single member has no election to wait for, so wait on it answering.
    local waited=0
    until netns_put 1 /registry/before-upgrade original >/dev/null 2>&1; do
        sleep 2; waited=$((waited + 2))
        [[ "$waited" -ge 40 ]] && die "the single member never accepted a write — log: $(_member1_log)"
    done
    assert_eq "$(netns_get 1 /registry/before-upgrade true)" "original" "the single member must hold what it was given"

    # It genuinely has no raft log. If this file existed the test would be
    # proving nothing, because the hard case is precisely its absence.
    [[ ! -f "$UPGRADE_ROOT/1/data/raft.db" ]] \
        || die "this member already has a raft log, so it is not the single-member case this test exists for"

    netns_kill_member 1
    sleep 2
    _restart_as_one_member_cluster

    local leader
    leader="$(netns_wait_for_leader 1 45)" \
        || die "the converted member never led its own one-member cluster — log: $(_member1_log)"
    assert_eq "$leader" "1" "the only voter must be the leader"

    # The whole point: the data is still there, and it is still there *through
    # raft* rather than because nothing changed.
    assert_eq "$(netns_get 1 /registry/before-upgrade true)" "original" \
        "the converted member must still hold what it held as a single member"
    netns_put 1 /registry/after-upgrade committed-by-raft >/dev/null
    assert_eq "$(netns_get 1 /registry/after-upgrade true)" "committed-by-raft" \
        "the converted member must accept new writes as a raft leader"

    _upgrade_stop
    trap - EXIT
}

# The dangerous shortcut, and why it is refused: two empty members are a
# majority and can elect each other without ever consulting the one that holds
# everything. The new leader then overwrites it — correctly, by raft's rules.
# Refusing to start is the only safe answer, and the message has to say what to
# do instead or the operator will simply delete the data.
test_upgrade_straight_to_a_multi_member_cluster_is_refused() {
    _upgrade_prepare
    trap _upgrade_stop EXIT

    _start_as_single_member
    local waited=0
    until netns_put 1 /registry/precious data >/dev/null 2>&1; do
        sleep 2; waited=$((waited + 2))
        [[ "$waited" -ge 40 ]] && die "the single member never accepted a write — log: $(_member1_log)"
    done
    netns_kill_member 1
    sleep 2

    # Now the mistake: point it straight at a three-member cluster.
    netns_start_member 1 "$UPGRADE_SIZE" "$UPGRADE_BIN" "$UPGRADE_ROOT" >/dev/null
    # Poll rather than sleeping a fixed interval: on a loaded runner the member
    # can take longer than any constant to get as far as refusing, and this
    # test would then fail on a missing string and name the wrong cause.
    waited=0
    until _member1_log | grep -q MemberAdd; do
        sleep 1; waited=$((waited + 1))
        [[ "$waited" -ge 30 ]] && break
    done

    local log; log="$(_member1_log)"
    assert_contains "$log" "MemberAdd" "the refusal must name the safe path, or the data gets deleted instead"
    # And it must not have come up serving anyway.
    assert_true test -f "$UPGRADE_ROOT/1/data/state.db"
    if netns_put 1 /registry/should-not-work x >/dev/null 2>&1; then
        die "the member started and accepted a write despite the unsafe configuration — log: $log"
    fi

    _upgrade_stop
    trap - EXIT
}

# Related to bug 3, and honest about how far it gets: this asserts that a
# member which has stopped leaves nothing of itself behind — no process in its
# namespace, nothing still answering on its client port.
#
# It does **not** reproduce bug 3 itself. That bug was a panic inside the raft
# driver's tokio task, which unwound only that task and left the process up,
# the listener open and `systemctl is-active` reporting active while every
# request failed. The fix is `install_fatal_panic_hook` in
# crates/nodestore/src/lib.rs, and nothing this harness can send makes the
# driver panic: the specific panic that was seen live
# (`to_commit N is out of range [last_index 0]`, an empty member rejoining a
# running cluster) is now refused at startup by the guard in
# replication/driver.rs, and there is no fault-injection knob to force another.
#
# The nearby storage-failure path is deliberately *not* a substitute for it:
# when `on_ready` fails, the driver stops but the process stays up on purpose,
# demoted to a follower with no leader so every handle reports unavailable —
# see the comment at that `return` in replication/driver.rs. Asserting an exit
# there would be asserting the opposite of the intended behaviour.
#
# So what remains testable is the consequence: once a member is gone, it is
# wholly gone.
test_upgrade_a_dead_member_leaves_nothing_listening_behind_it() {
    _upgrade_prepare
    trap _upgrade_stop EXIT

    _start_as_single_member
    local waited=0
    until netns_put 1 /registry/alive yes >/dev/null 2>&1; do
        sleep 2; waited=$((waited + 2))
        [[ "$waited" -ge 40 ]] && die "the member never came up — log: $(_member1_log)"
    done

    netns_kill_member 1
    sleep 3
    if netns_get 1 /registry/alive true 2>/dev/null | grep -q yes; then
        die "something is still serving the client API after the member was killed"
    fi
    assert_eq "$(ip netns pids "$(netns_name 1)" 2>/dev/null | wc -l)" "0" \
        "no process may survive in the member's namespace — a half-dead member is worse than a dead one"

    _upgrade_stop
    trap - EXIT
}

register_test test_upgrade_a_populated_single_member_into_a_one_member_cluster
register_test test_upgrade_straight_to_a_multi_member_cluster_is_refused
register_test test_upgrade_a_dead_member_leaves_nothing_listening_behind_it
