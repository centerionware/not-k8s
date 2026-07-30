# lib/test/cases/resources.sh — resource requests/limits actually reaching
# cgroups, not just being advertised in the Pod spec. Reads the container's
# own cgroup v2 files (cat'd into a shared emptyDir — see dns.sh for why
# this needs the container's own cooperation rather than a host-readable
# path). Assumes cgroup v2 (the modern containerd default); skips cleanly
# if that's not what this node uses rather than guessing at the v1 paths.

test_memory_limit_is_enforced_via_cgroup() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="mem-limit"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        limits:
          memory: "67108864"
      command: ["sh", "-c", "cat /sys/fs/cgroup/memory.max > /shared/memmax.txt 2>/shared/memmax.err; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local value
    if ! value="$(try_wait_until 30 bash -c "[[ -s \"\$(pod_volume_host_path '$name' shared)/memmax.txt\" ]]" && wait_for_check_file "$name" shared memmax.txt 5)"; then
        delete_pod_if_exists "$name"
        skip_test "no /sys/fs/cgroup/memory.max in the container — this node likely uses cgroup v1, not v2"
    fi
    assert_eq "$value" "67108864" "cgroup memory.max should match the Pod's memory limit exactly"
    delete_pod_if_exists "$name"
}

test_cpu_limit_is_enforced_via_cgroup() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="cpu-limit"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        limits:
          cpu: "250m"
      command: ["sh", "-c", "cat /sys/fs/cgroup/cpu.max > /shared/cpumax.txt 2>/shared/cpumax.err; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local value
    if ! value="$(try_wait_until 30 bash -c "[[ -s \"\$(pod_volume_host_path '$name' shared)/cpumax.txt\" ]]" && wait_for_check_file "$name" shared cpumax.txt 5)"; then
        delete_pod_if_exists "$name"
        skip_test "no /sys/fs/cgroup/cpu.max in the container — this node likely uses cgroup v1, not v2"
    fi
    # 250m -> cpu_period_us default 100000, quota = 100000 * 250 / 1000 = 25000.
    assert_eq "$value" "25000 100000" "cgroup cpu.max should be '<quota> <period>' matching linux_resources()'s formula"
    delete_pod_if_exists "$name"
}

test_besteffort_pod_gets_no_cgroup_limit() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="besteffort"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /sys/fs/cgroup/memory.max > /shared/memmax.txt 2>/shared/memmax.err; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local value
    if ! value="$(try_wait_until 30 bash -c "[[ -s \"\$(pod_volume_host_path '$name' shared)/memmax.txt\" ]]" && wait_for_check_file "$name" shared memmax.txt 5)"; then
        delete_pod_if_exists "$name"
        skip_test "no /sys/fs/cgroup/memory.max in the container — this node likely uses cgroup v1, not v2"
    fi
    assert_eq "$value" "max" "a BestEffort pod (no memory limit set) must not get an accidental cgroup ceiling"
    delete_pod_if_exists "$name"
}

register_test test_memory_limit_is_enforced_via_cgroup
register_test test_cpu_limit_is_enforced_via_cgroup
register_test test_besteffort_pod_gets_no_cgroup_limit
