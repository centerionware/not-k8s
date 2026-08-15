# lib/test/cases/cpu_manager.sh — CPU Manager static policy
# (cpu_manager.rs): Guaranteed-QoS containers requesting a whole number of
# CPUs get pinned to exclusive cores via cpuset.cpus, instead of sharing
# the CFS-scheduled pool with everything else — and (round 16)
# already-running shared-pool containers get retroactively shrunk/grown
# via CRI's UpdateContainerResources as exclusive claims are made/released.
# Opt-in on the running nodelet (NODELET_CPU_MANAGER_POLICY=static) —
# round 123: nodelet_restart_with_env (nodelet_env.sh) actually restarts
# nodelet with that set for the duration of each test below, instead of
# relying on an externally pre-configured nodelet + a TEST_CPU_MANAGER_STATIC
# hint that nothing in CI ever set.

# Round found live in CI, with RUST_LOG=nodescheduler=debug (e2e.yml's
# debug_scheduler_log input) actually turned on: check-b's own "Insufficient
# cpu" rejections carried real committed/requested/allocatable numbers this
# time, not just the reason string, and they didn't add up to a capacity
# problem — committed=3510/allocatable=4000 while `kubectl describe node`
# and the real pod list agreed only ~1110m was actually in use. The 2400m
# difference is exactly the previous test's own pod
# (test_scheduler_does_not_preempt_when_policy_forbids_it's $low, 60% of
# allocatable) — genuinely deleted well before this test starts (that test's
# own delete_pod_and_wait_gone confirms it via a real `kubectl get`, and
# nodescheduler's pod watch shows no reconnect/relist in this whole window,
# ruling out the exact bug #19 fixed) — but its resources stayed committed
# in nodescheduler's cache for up to several minutes after being confirmed
# gone via the API. Whatever the real mechanism (nodestore's watch/commit
# path under the concurrent write load of a dozen other tests running in
# the same window is the leading suspect, unconfirmed), it's a genuine
# propagation delay, not a leak — it always did clear on its own, just far
# slower than this test's original 90s budget assumed. 240s is generous
# headroom over the worst delay observed (~3.5min); this is a documented
# mitigation for a real, still-open timing issue, not a root-cause fix —
# see this comment if it starts happening again even at this budget.
CPU_MANAGER_PIN_TIMEOUT_SECS="${CPU_MANAGER_PIN_TIMEOUT_SECS:-240}"

test_cpu_manager_pins_guaranteed_containers_to_disjoint_exclusive_cores() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to restart nodelet with NODELET_CPU_MANAGER_POLICY=static"; fi
    cpu_manager_pin_test_cleanup() { nodelet_restore_env; }
    trap cpu_manager_pin_test_cleanup EXIT
    nodelet_restart_with_env "NODELET_CPU_MANAGER_POLICY=static"

    local cgroup_root="${NODELET_CGROUP_FS_ROOT:-/sys/fs/cgroup}"
    local name_a="cpu-manager-check-a"
    local name_b="cpu-manager-check-b"

    for name in "$name_a" "$name_b"; do
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
      resources:
        requests: { cpu: "1", memory: "64Mi" }
        limits: { cpu: "1", memory: "64Mi" }
EOF
    done
    wait_until "$CPU_MANAGER_PIN_TIMEOUT_SECS" "$name_a Running" pod_is_phase "$name_a" Running
    wait_until "$CPU_MANAGER_PIN_TIMEOUT_SECS" "$name_b Running" pod_is_phase "$name_b" Running

    local cid_a cid_b path_a path_b cpuset_a cpuset_b
    cid_a="$(kctl get pod "$name_a" -o jsonpath='{.status.containerStatuses[0].containerID}' | sed 's#.*://##')"
    cid_b="$(kctl get pod "$name_b" -o jsonpath='{.status.containerStatuses[0].containerID}' | sed 's#.*://##')"

    path_a="$(find "$cgroup_root" -maxdepth 8 -type d -iname "*${cid_a}*" 2>/dev/null | head -1)"
    path_b="$(find "$cgroup_root" -maxdepth 8 -type d -iname "*${cid_b}*" 2>/dev/null | head -1)"
    if [[ -z "$path_a" || -z "$path_b" ]]; then
        delete_pod_if_exists "$name_a"
        delete_pod_if_exists "$name_b"
        skip_test "couldn't find a per-container cgroup directory under $cgroup_root by container ID — this containerd's cgroup layout may differ from what this test expects"
    fi

    cpuset_a="$(cat "$path_a/cpuset.cpus" 2>/dev/null)"
    cpuset_b="$(cat "$path_b/cpuset.cpus" 2>/dev/null)"
    assert_not_empty "$cpuset_a" "container A has a cpuset.cpus"
    assert_not_empty "$cpuset_b" "container B has a cpuset.cpus"

    if [[ "$cpuset_a" == "$cpuset_b" ]]; then
        delete_pod_if_exists "$name_a"
        delete_pod_if_exists "$name_b"
        die "two Guaranteed 1-CPU pods got the identical cpuset ($cpuset_a) — CPU Manager should have assigned disjoint exclusive cores, check cpu_manager.rs::allocate() and its wiring in runtime/cri.rs::create_and_start_container()"
    fi

    delete_pod_if_exists "$name_a"
    delete_pod_if_exists "$name_b"
}

test_cpu_manager_retroactively_shrinks_an_already_running_shared_pool_container() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to restart nodelet with NODELET_CPU_MANAGER_POLICY=static"; fi
    cpu_manager_shrink_test_cleanup() { nodelet_restore_env; }
    trap cpu_manager_shrink_test_cleanup EXIT
    nodelet_restart_with_env "NODELET_CPU_MANAGER_POLICY=static"

    local cgroup_root="${NODELET_CGROUP_FS_ROOT:-/sys/fs/cgroup}"
    local shared_name="cpu-manager-shared-check"
    local exclusive_name="cpu-manager-exclusive-check"

    # BestEffort: no resources at all, so it lands in the shared pool.
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $shared_name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    wait_until 90 "$shared_name Running" pod_is_phase "$shared_name" Running

    local shared_cid shared_path cpuset_before
    shared_cid="$(kctl get pod "$shared_name" -o jsonpath='{.status.containerStatuses[0].containerID}' | sed 's#.*://##')"
    shared_path="$(find "$cgroup_root" -maxdepth 8 -type d -iname "*${shared_cid}*" 2>/dev/null | head -1)"
    if [[ -z "$shared_path" ]]; then
        delete_pod_if_exists "$shared_name"
        skip_test "couldn't find the shared-pool container's cgroup directory under $cgroup_root by container ID"
    fi
    cpuset_before="$(cat "$shared_path/cpuset.cpus" 2>/dev/null)"
    assert_not_empty "$cpuset_before" "shared-pool container has an initial cpuset.cpus"

    # Now claim an exclusive core with a Guaranteed 1-CPU pod — this should
    # trigger refresh_shared_pool_cpusets() to shrink the already-running
    # BestEffort container's cpuset.
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $exclusive_name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      resources:
        requests: { cpu: "1", memory: "64Mi" }
        limits: { cpu: "1", memory: "64Mi" }
EOF
    wait_until 90 "$exclusive_name Running" pod_is_phase "$exclusive_name" Running

    local cpuset_after
    if ! try_wait_until 90 bash -c "[[ \"\$(cat '$shared_path/cpuset.cpus' 2>/dev/null)\" != '$cpuset_before' ]]"; then
        delete_pod_if_exists "$shared_name"
        delete_pod_if_exists "$exclusive_name"
        die "the shared-pool container's cpuset.cpus never changed after a new exclusive claim — check refresh_shared_pool_cpusets() in runtime/cri.rs and its UpdateContainerResources call"
    fi
    cpuset_after="$(cat "$shared_path/cpuset.cpus" 2>/dev/null)"
    assert_not_empty "$cpuset_after" "shared-pool container still has a cpuset.cpus after the refresh"

    delete_pod_if_exists "$shared_name"
    delete_pod_if_exists "$exclusive_name"
}

register_test test_cpu_manager_pins_guaranteed_containers_to_disjoint_exclusive_cores
register_test test_cpu_manager_retroactively_shrinks_an_already_running_shared_pool_container
