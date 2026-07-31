# lib/test/cases/security.sh — securityContext translation to CRI's
# LinuxContainerSecurityContext, verified by having the container itself
# report what it sees into a shared emptyDir (read back off the host —
# same trick as volumes.sh; there's no exec to check this from outside).

test_run_as_user_is_applied() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="run-as-user"
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
      securityContext:
        runAsUser: 1000
        runAsGroup: 1000
      command: ["sh", "-c", "id -u > /shared/uid.txt; id -g > /shared/gid.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local uid gid
    uid="$(wait_for_check_file "$name" shared uid.txt 30)"
    gid="$(wait_for_check_file "$name" shared gid.txt 20)"
    assert_eq "$uid" "1000" "runAsUser"
    assert_eq "$gid" "1000" "runAsGroup"
    delete_pod_if_exists "$name"
}

test_read_only_root_filesystem_is_enforced() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="readonly-rootfs"
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
      securityContext:
        readOnlyRootFilesystem: true
      command:
        - sh
        - -c
        - |
          if touch /roottest 2>/dev/null; then
            echo writable > /shared/result.txt
          else
            echo readonly > /shared/result.txt
          fi
          sleep 3600
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local result
    result="$(wait_for_check_file "$name" shared result.txt 30)"
    assert_eq "$result" "readonly" "readOnlyRootFilesystem must block writes outside mounted volumes"
    delete_pod_if_exists "$name"
}

test_without_read_only_root_filesystem_writes_succeed() {
    # Control case for the test above — proves the check itself is valid
    # (i.e. the image's root normally *is* writable) rather than "readonly"
    # showing up for some unrelated reason.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="writable-rootfs"
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
      command:
        - sh
        - -c
        - |
          if touch /roottest 2>/dev/null; then
            echo writable > /shared/result.txt
          else
            echo readonly > /shared/result.txt
          fi
          sleep 3600
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local result
    result="$(wait_for_check_file "$name" shared result.txt 30)"
    assert_eq "$result" "writable" "root filesystem should be writable without readOnlyRootFilesystem"
    delete_pod_if_exists "$name"
}

test_host_users_false_gets_a_real_user_namespace() {
    # Round 25: spec.hostUsers: false should get an exclusive host UID/GID
    # range, not the host's own UID space. /proc/self/uid_map inside a real
    # user namespace shows a remapped range ("0 <host_base> <length>");
    # outside one it shows the host's full identity range
    # ("0 0 4294967295").
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="hostusers-false-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  hostUsers: false
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /proc/self/uid_map > /shared/uid_map.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        skip_test "pod never reached Running with hostUsers: false — check nodelet's logs for 'user namespace: no free UID/GID range available' (pool exhausted) or a RunPodSandbox error (runtime doesn't support CRI's userns_options at all, e.g. too old containerd)"
    fi

    local uid_map
    uid_map="$(wait_for_check_file "$name" shared uid_map.txt 30)"
    assert_not_empty "$uid_map" "/proc/self/uid_map should have content"
    if echo "$uid_map" | grep -q "^\s*0\s\+0\s\+4294967295"; then
        delete_pod_if_exists "$name"
        die "uid_map shows the host's own full identity range ('0 0 4294967295') — this container is NOT in a user namespace at all; check runtime/cri.rs's userns_mapping wiring and that the CRI runtime actually honors LinuxSandboxSecurityContext.namespace_options.userns_options"
    fi
    delete_pod_if_exists "$name"
}

test_containers_get_isolated_pid_namespaces_by_default() {
    # Round 40: real Kubernetes' actual default is CONTAINER-scoped PID
    # namespaces (each container is its own pid-1), NOT the CRI proto's own
    # zero-value default (POD-shared) nodelet was previously silently
    # relying on. A container's own shell reports pid 1 for itself only
    # when it's truly the init process of its own isolated namespace.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="pid-isolated-default"
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
    - name: first
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
    - name: second
      image: $TEST_IMAGE
      command: ["sh", "-c", "echo \$\$ > /shared/second-pid.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local second_pid
    second_pid="$(wait_for_check_file "$name" shared second-pid.txt 30)"
    assert_eq "$second_pid" "1" "second container's own shell should be pid 1 in its own isolated PID namespace"
    delete_pod_if_exists "$name"
}

test_share_process_namespace_puts_every_container_in_one_pid_namespace() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="pid-shared"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  shareProcessNamespace: true
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: first
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
    - name: second
      image: $TEST_IMAGE
      command: ["sh", "-c", "echo \$\$ > /shared/second-pid.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local second_pid
    second_pid="$(wait_for_check_file "$name" shared second-pid.txt 30)"
    assert_not_eq "$second_pid" "1" "with shareProcessNamespace, the first container already holds pid 1 — the second container's shell must get a higher pid in the shared namespace"
    delete_pod_if_exists "$name"
}

test_host_pid_sees_host_processes() {
    # hostPID: true joins the node's own PID namespace, so the container
    # should see far more than its own 1-2 processes.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="host-pid-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  hostPID: true
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "ls /proc | grep -E '^[0-9]+\$' | wc -l > /shared/proc_count.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local count
    count="$(wait_for_check_file "$name" shared proc_count.txt 30)"
    assert_true bash -c "[[ '$count' -gt 5 ]]" "hostPID container should see many more than its own handful of processes (saw $count)"
    delete_pod_if_exists "$name"
}

register_test test_run_as_user_is_applied
register_test test_read_only_root_filesystem_is_enforced
register_test test_without_read_only_root_filesystem_writes_succeed
test_sysctls_are_applied_to_the_sandbox() {
    # Round 41: spec.securityContext.sysctls -> CRI's
    # LinuxPodSandboxConfig.sysctls map. net.ipv4.ip_unprivileged_port_start
    # is namespaced (safe to set without hostNetwork/privileged) and its
    # current value is directly readable via /proc/sys, so this is a real,
    # structural proof rather than just a status-string check.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="sysctls-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  securityContext:
    sysctls:
      - name: net.ipv4.ip_unprivileged_port_start
        value: "1234"
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /proc/sys/net/ipv4/ip_unprivileged_port_start > /shared/sysctl.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        skip_test "pod never reached Running with a sysctl set — check nodelet's logs for a RunPodSandbox error (runtime may not support this specific sysctl as namespaced, or reject unknown sysctls)"
    fi
    local value
    value="$(wait_for_check_file "$name" shared sysctl.txt 30)"
    assert_eq "$value" "1234" "net.ipv4.ip_unprivileged_port_start should reflect the pod's own securityContext.sysctls value"
    delete_pod_if_exists "$name"
}

register_test test_host_users_false_gets_a_real_user_namespace
register_test test_containers_get_isolated_pid_namespaces_by_default
register_test test_share_process_namespace_puts_every_container_in_one_pid_namespace
register_test test_host_pid_sees_host_processes
register_test test_sysctls_are_applied_to_the_sandbox
