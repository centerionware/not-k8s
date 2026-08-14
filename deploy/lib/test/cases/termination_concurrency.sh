# lib/test/cases/termination_concurrency.sh — terminating one pod must not
# stop this node from doing anything else.
#
# pods.rs drives everything from a single serial select! loop: one watch
# event, or one runtime event, is handled to completion before the next is
# looked at. That is the right shape for a component whose whole premise is
# not polling — but it makes any long await inside a handler a node-wide
# stall, not a per-pod one.
#
# Teardown is the long one, and it is long by design rather than by accident:
# remove_pod() issues a CRI StopContainer per container, and StopContainer
# waits out the pod's terminationGracePeriodSeconds before killing. Awaited
# inline, deleting a single pod with a 60s grace period meant this node
# created no pods, wrote no statuses, and handled no probe or runtime events
# for a full minute — while reporting itself perfectly healthy the entire
# time. Nothing surfaces as an error; unrelated pods just take inexplicably
# long to start, which is exactly the "pods intermittently taking far longer
# than expected to reach Running in CI" that reconcile()'s own comment had
# already noticed and not explained.
#
# The test is therefore deliberately a *timing* assertion, because the bug is
# only a timing one — with teardown awaited inline this fails, and with it
# detached it passes, and nothing else about the observable state differs
# between the two.

# A container that ignores SIGTERM, so the grace period is really spent
# rather than short-circuited by a well-behaved process exiting at once.
# `trap '' TERM` in the shell, then sleep: SIGTERM is discarded, and the
# runtime has no choice but to wait the full grace period and SIGKILL.
_slow_terminating_pod() { # _slow_terminating_pod <name> <grace-seconds>
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $1
spec:
  terminationGracePeriodSeconds: $2
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "trap '' TERM; sleep 3600"]
EOF
}

test_a_slow_terminating_pod_does_not_stall_another_pods_creation() {
    if ! node_uses_cri_runtime; then
        skip_test "needs the cri runtime — the mock one has no real StopContainer to wait on"
    fi

    local blocker="term-blocker" victim="term-victim"
    # Long enough that an inline teardown is unmistakable against the
    # normal time-to-Running, short enough not to dominate the suite.
    local grace=45

    _slow_terminating_pod "$blocker" "$grace"
    wait_until 90 "$blocker Running" pod_is_phase "$blocker" Running

    # Start the clock before the delete so the measurement covers the whole
    # window in which the node is supposed to still be responsive.
    local started_s=$SECONDS
    kctl delete pod "$blocker" --grace-period="$grace" --wait=false >/dev/null 2>&1 || true

    # Give the delete's watch event time to actually reach nodelet and enter
    # teardown. Without this the victim could be created before the stall
    # even begins, and the test would pass against the broken code too.
    sleep 5

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $victim
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF

    # The budget is the point. A healthy node starts this pod in a few
    # seconds; a node stalled inside teardown cannot start it until the
    # blocker's grace period expires, which is at least $grace from the
    # delete above. Waiting strictly less than that means reaching Running
    # is only possible if the teardown was NOT holding the loop.
    if ! try_wait_until $((grace - 15)) pod_is_phase "$victim" Running; then
        delete_pod_if_exists "$blocker"
        delete_pod_if_exists "$victim"
        die "an unrelated pod could not be created while another was terminating — nodelet's event loop is blocked in teardown() for the whole terminationGracePeriodSeconds (${grace}s here); it should be detached onto its own task"
    fi
    local elapsed_s=$((SECONDS - started_s))

    # Cheap belt-and-braces on the measurement itself: if the whole thing
    # somehow took longer than the grace period, the assertion above proved
    # nothing, because the blocker would have finished terminating anyway.
    if (( elapsed_s >= grace )); then
        delete_pod_if_exists "$blocker"
        delete_pod_if_exists "$victim"
        die "the victim pod reached Running after ${elapsed_s}s, at or past the blocker's ${grace}s grace period — this run proves nothing either way, rather than proving the loop was free"
    fi

    delete_pod_if_exists "$blocker"
    delete_pod_if_exists "$victim"
}

register_test test_a_slow_terminating_pod_does_not_stall_another_pods_creation
