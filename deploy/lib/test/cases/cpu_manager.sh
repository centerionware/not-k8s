# lib/test/cases/cpu_manager.sh — CPU Manager static policy
# (cpu_manager.rs): Guaranteed-QoS containers requesting a whole number of
# CPUs get pinned to exclusive cores via cpuset.cpus, instead of sharing
# the CFS-scheduled pool with everything else. Opt-in on the running
# nodelet (NODELET_CPU_MANAGER_POLICY=static) — skips cleanly without
# TEST_CPU_MANAGER_STATIC=true telling this suite that's actually the case,
# same pattern log_rotation.sh/static_pods.sh use for their own opt-in
# NODELET_* settings.

test_cpu_manager_pins_guaranteed_containers_to_disjoint_exclusive_cores() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ "${TEST_CPU_MANAGER_STATIC:-}" != "true" ]]; then
        skip_test "TEST_CPU_MANAGER_STATIC not set to 'true' — export it once nodelet is running with NODELET_CPU_MANAGER_POLICY=static to exercise this"
    fi
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
    wait_until 30 "$name_a Running" pod_is_phase "$name_a" Running
    wait_until 30 "$name_b Running" pod_is_phase "$name_b" Running

    local cid_a cid_b path_a path_b cpuset_a cpuset_b
    cid_a="$(kubectl get pod "$name_a" -o jsonpath='{.status.containerStatuses[0].containerID}' | sed 's#.*://##')"
    cid_b="$(kubectl get pod "$name_b" -o jsonpath='{.status.containerStatuses[0].containerID}' | sed 's#.*://##')"

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

register_test test_cpu_manager_pins_guaranteed_containers_to_disjoint_exclusive_cores
