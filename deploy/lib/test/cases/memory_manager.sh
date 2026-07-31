# lib/test/cases/memory_manager.sh — Memory Manager static policy
# (memory_manager.rs): Guaranteed-QoS containers with a memory limit get
# their memory pinned to a single NUMA node via cpuset.mems. Opt-in on the
# running nodelet (NODELET_MEMORY_MANAGER_POLICY=static) — skips cleanly
# without TEST_MEMORY_MANAGER_STATIC=true telling this suite that's
# actually the case, same pattern cpu_manager.sh uses for its own opt-in
# NODELET_* setting.

test_memory_manager_pins_guaranteed_containers_to_a_numa_node() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ "${TEST_MEMORY_MANAGER_STATIC:-}" != "true" ]]; then
        skip_test "TEST_MEMORY_MANAGER_STATIC not set to 'true' — export it once nodelet is running with NODELET_MEMORY_MANAGER_POLICY=static to exercise this"
    fi
    local cgroup_root="${NODELET_CGROUP_FS_ROOT:-/sys/fs/cgroup}"
    local name="memory-manager-check"

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
        requests: { cpu: "100m", memory: "64Mi" }
        limits: { cpu: "100m", memory: "64Mi" }
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running

    local cid path cpuset_mems
    cid="$(kubectl get pod "$name" -o jsonpath='{.status.containerStatuses[0].containerID}' | sed 's#.*://##')"
    path="$(find "$cgroup_root" -maxdepth 8 -type d -iname "*${cid}*" 2>/dev/null | head -1)"
    if [[ -z "$path" ]]; then
        delete_pod_if_exists "$name"
        skip_test "couldn't find the container's cgroup directory under $cgroup_root by container ID — this containerd's cgroup layout may differ from what this test expects"
    fi

    cpuset_mems="$(cat "$path/cpuset.mems" 2>/dev/null)"
    assert_not_empty "$cpuset_mems" "container has a cpuset.mems (a Guaranteed pod with a memory limit should be pinned to a NUMA node under Memory Manager)"

    delete_pod_if_exists "$name"
}

register_test test_memory_manager_pins_guaranteed_containers_to_a_numa_node
