# lib/test/cases/retry_backoff.sh — a pod recovers on its own once a
# node-level failure is fixed.
#
# pods.rs is watch-driven with no resync loop, which is the entire point of
# this project: nothing polls, so cost is proportional to what changes. The
# cost of that design is that a failure which is *not* caused by a Pod
# mutation also produces no event to recover from. Found live: a node whose
# cgroups were never mounted failed every sandbox in runc, and after mounting
# them the pods stayed Pending indefinitely, because the Pod objects
# themselves never changed. Only restarting nodelet cleared it.
#
# schedule_retry()'s exponential backoff is what closes that, and this is the
# test that it actually does — end to end, against real containerd, with the
# failure introduced and then repaired underneath a pod that is already bound.
#
# Stopping containerd is the same shape of failure as the unmounted cgroups
# (every sandbox creation fails at the runtime, with the Pod object untouched)
# and is something this suite can genuinely stand up and tear down again,
# unlike unmounting the host's cgroup hierarchy mid-run.
#
# Restarting containerd puts this in the class harness.sh defers to the end of
# the run — see _reorder_env_reconfiguring_tests_last().

test_a_pending_pod_recovers_after_the_node_failure_is_fixed() {
    if ! node_uses_cri_runtime; then skip_test "needs the cri runtime — the mock one has no sandbox to fail"; fi
    command -v systemctl >/dev/null 2>&1 || skip_test "needs systemd to stop and start containerd"
    sudo systemctl is-active --quiet containerd 2>/dev/null \
        || skip_test "containerd is not a running systemd unit on this host"

    local name="retry-after-repair"

    cleanup() {
        sudo systemctl restart containerd &>/dev/null || true
        try_wait_until 60 bash -c "sudo ctr version &>/dev/null" || true
        delete_pod_if_exists "$name" || true
    }
    trap cleanup EXIT

    delete_pod_if_exists "$name"

    # Break the node underneath the runtime. Every ensure_pod() for a new pod
    # now fails in the CRI call, which is exactly the failure being modelled.
    log "stopping containerd so every sandbox creation fails..."
    sudo systemctl stop containerd

    # Bound to this node explicitly. With containerd down the node goes
    # NotReady within a heartbeat or two and the scheduler would refuse to
    # place the pod at all — which would prove nothing about nodelet.
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  nodeName: $(node_name)
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF

    # It must NOT come up while the node is broken — otherwise the rest of
    # this proves nothing, because there was no failure to recover from.
    if try_wait_until 30 pod_is_phase "$name" Running; then
        die "pod reached Running with containerd stopped — the failure this test depends on never happened"
    fi

    # Repair the node. Nothing touches the Pod object, so there is no watch
    # event and no relist: recovery can only come from schedule_retry()'s own
    # backoff. Before the backoff existed, the single 5s retry had been spent
    # long ago and this pod stayed Pending until nodelet was restarted.
    log "starting containerd again — nothing touches the Pod, so only nodelet's own retry can recover it..."
    sudo systemctl start containerd
    try_wait_until 60 bash -c "sudo ctr version &>/dev/null" \
        || die "containerd never came back up, so this test cannot tell recovery from a still-broken node"

    # Generous budget on purpose: by now the delay has backed off several
    # steps (5s, 10s, 20s, 40s, ...), so recovery is not immediate by design.
    # What matters is that it happens at all, with no nodelet restart and no
    # mutation of the Pod.
    wait_until 300 "$name Running again after the node was repaired" pod_is_phase "$name" Running
}

register_test test_a_pending_pod_recovers_after_the_node_failure_is_fixed
