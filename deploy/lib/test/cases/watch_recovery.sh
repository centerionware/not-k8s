# lib/test/cases/watch_recovery.sh — the node keeps working across an
# apiserver restart.
#
# This is the failure mode that has cost this project the most time per unit
# of code, because every component reports itself healthy throughout: the
# node stays Ready (its Lease is renewed by a different task), the apiserver
# is fine, the scheduler is fine and has already set spec.nodeName — and pods
# simply sit Pending forever because the node agent's watch never came back.
# It reads as a scheduling problem and is not one.
#
# The trigger is real and routine rather than exotic: deploy/setup-control-plane.sh
# restarts k3s on its second pass (adding the kubelet CA), and any operator
# restarting their control plane does the same thing. During that window
# every watch *start* fails immediately, which is a different thing from a
# watch being interrupted — kube's watcher self-heals from the second and
# does nothing to pace the first. Bare watchers therefore busy-loop against a
# server that is mid-startup: 128 requests in one second, measured live.
#
# Asserting on the recovery rather than on the spin is deliberate. Request
# rate is awkward to measure portably and would be a flaky thing to gate on;
# "a pod created after the restart actually runs" is the property anyone
# actually cares about, it fails outright when the watch is wedged, and it
# needs no instrumentation.
#
# Restarting k3s puts this in the class harness.sh defers to the end of the
# run — see _reorder_env_reconfiguring_tests_last().

_apiserver_is_serving() {
    kubectl get --raw /readyz >/dev/null 2>&1
}

test_the_node_still_reconciles_pods_after_an_apiserver_restart() {
    if ! node_uses_cri_runtime; then skip_test "needs the cri runtime — the mock one never talks to a real apiserver"; fi
    command -v systemctl >/dev/null 2>&1 || skip_test "needs systemd to restart k3s"
    systemctl list-unit-files k3s.service >/dev/null 2>&1 || skip_test "no k3s.service on this host to restart"

    local name="watch-recovery-check"
    delete_pod_if_exists "$name"

    # Restart the control plane out from under the running node agent. This
    # is the exact event that wedged it: the watches are live, then every
    # attempt to re-establish them fails for several seconds.
    sudo systemctl restart k3s \
        || die "could not restart k3s — this test cannot exercise anything without doing so"

    local waited=0
    until _apiserver_is_serving; do
        waited=$((waited + 2))
        [[ "$waited" -gt 180 ]] && die "the apiserver never came back after a restart — nothing below is meaningful, and this is a control-plane problem rather than a node-agent one"
        sleep 2
    done

    # The default ServiceAccount is recreated with the namespace's own
    # lifecycle, but a restarted apiserver can briefly answer before its
    # controllers have caught up — without this, the create below can be
    # rejected with a Forbidden that reads as a watch failure. Same race
    # test-e2e.sh guards at startup, for the same reason.
    local sa_waited=0
    until kubectl -n "$TEST_NAMESPACE" get serviceaccount default >/dev/null 2>&1; do
        sa_waited=$((sa_waited + 2))
        [[ "$sa_waited" -gt 60 ]] && die "the default ServiceAccount never reappeared after the restart, so a pod create here would be rejected for a reason unrelated to what this test measures"
        sleep 2
    done

    # THE assertion. This pod is created entirely after the restart, so the
    # node agent can only ever learn about it through a watch that
    # reconnected. A wedged watch means it stays Pending until something
    # restarts nodelet.
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF

    if ! try_wait_until 120 pod_is_phase "$name" Running; then
        local node_name phase
        node_name="$(pod_field "$name" '{.spec.nodeName}')"
        phase="$(pod_field "$name" '{.status.phase}')"
        delete_pod_if_exists "$name"
        if [[ -n "$node_name" ]]; then
            die "pod was bound to '$node_name' but never left phase '$phase' after an apiserver restart — the scheduler did its half, so the node agent's pod watch did not recover. Check nodelet's log for a burst of watch errors around the restart and then silence: journalctl -u nodelet"
        fi
        die "pod never reached Running after an apiserver restart and was never even bound (phase '$phase') — that is a scheduling or control-plane failure rather than the node-agent watch recovery this test is about"
    fi

    delete_pod_if_exists "$name"
}

register_test test_the_node_still_reconciles_pods_after_an_apiserver_restart
