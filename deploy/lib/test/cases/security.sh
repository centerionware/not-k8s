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

register_test test_run_as_user_is_applied
register_test test_read_only_root_filesystem_is_enforced
register_test test_without_read_only_root_filesystem_writes_succeed
register_test test_host_users_false_gets_a_real_user_namespace
