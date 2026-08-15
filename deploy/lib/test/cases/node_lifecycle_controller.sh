# lib/test/cases/node_lifecycle_controller.sh — nodecontroller's Group A:
# node-ipam-controller (podCIDR allocation) and node-lifecycle-controller
# (taint after heartbeat loss). Neither has any e2e coverage anywhere else
# in this suite — until nodecontroller existed this was entirely k3s's
# bundled kube-controller-manager's job (see docs/CONTROLLER_MANAGER.md).
#
# Gated on CONTROLLER_MANAGER=nodecontroller actually being the thing
# running, the same "deliberately checks the *unit*, not the binary's
# presence" discipline cases/scheduler.sh's own _require_nodescheduler
# uses — a node that built nodecontroller but is still running k3s's
# bundled controller-manager must skip, or these tests would silently
# validate upstream instead of this project's own replacement.

_nodecontroller_is_running() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_require_nodecontroller() {
    _nodecontroller_is_running \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller (which also disables k3s's own controller manager) to exercise these"
}

node_taint_present() { # node_taint_present <taint-key>
    local key
    key="$(kubectl get node "$(node_name)" -o jsonpath="{.spec.taints[?(@.key==\"$1\")].key}" 2>/dev/null || true)"
    [[ "$key" == "$1" ]]
}

test_node_gets_a_pod_cidr_allocated() {
    _require_nodecontroller
    # If this node has ever come up successfully with a working CNI, it
    # already has one — this asserts nodecontroller (not k3s) is the thing
    # that put it there, which only matters when it's actually running.
    local cidr
    cidr="$(kubectl get node "$(node_name)" -o jsonpath='{.spec.podCIDR}')"
    assert_not_empty "$cidr" "Node.spec.podCIDR is set (allocated by nodecontroller's node-ipam-controller)"
}

# The real thing this test breaks and repairs: nodelet's Lease renewal
# (kube-node-lease, node-monitor-period=10s — see crates/nodelet/src/node.rs)
# is node-lifecycle-controller's only liveness signal (docs/CONTROLLER_MANAGER.md,
# Group A). Stopping nodelet is the only way to make that signal actually go
# stale for real, rather than asserting the taint logic in the abstract —
# same "break the real thing, prove the bad state actually happened, repair,
# assert recovery" shape as cases/retry_backoff.sh.
#
# NODECONTROLLER_NODE_MONITOR_GRACE_PERIOD_SECONDS is not overridden here —
# this test pays the full default grace period (40s) rather than reaching
# for a faster env-reconfigured nodecontroller, because reconfiguring
# nodecontroller for one test would also need it restarted with an override
# and restored afterward (nodelet_restart_with_env's own pattern), doubling
# this test's real disruption for a modest time saving.
test_node_is_tainted_unreachable_after_heartbeat_loss_and_recovers() {
    _require_nodecontroller
    if ! command -v systemctl >/dev/null 2>&1 || ! systemctl list-unit-files nodelet.service >/dev/null 2>&1; then
        skip_test "needs systemd (nodelet.service) to stop/start nodelet for real"
    fi

    local name
    name="$(node_name)"

    trap 'sudo systemctl start nodelet.service 2>/dev/null || true' EXIT

    sudo systemctl stop nodelet.service
    # Grace period (default 40s) plus jitter headroom (up to 5%) plus one
    # governor tick plus real margin for CI contention.
    wait_until 90 "node $name tainted unreachable after its heartbeat Lease went stale" \
        node_taint_present "node.kubernetes.io/unreachable"

    sudo systemctl start nodelet.service
    trap - EXIT
    wait_until 120 "node $name Ready again after nodelet restarted" \
        bash -c '[[ "$(node_condition_status Ready)" == "True" ]]'
    wait_until 60 "unreachable taint cleared after the heartbeat Lease resumed renewing" \
        bash -c '! node_taint_present "node.kubernetes.io/unreachable"'
}

register_test test_node_gets_a_pod_cidr_allocated
register_test test_node_is_tainted_unreachable_after_heartbeat_loss_and_recovers
