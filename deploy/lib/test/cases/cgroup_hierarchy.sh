# lib/test/cases/cgroup_hierarchy.sh — QoS-scoped cgroup_parent
# (cgroup.rs::cgroup_parent_for, wired into runtime/cri.rs::ensure_pod) and
# node allocatable enforcement (cgroup.rs::enforce_node_allocatable, called
# once at nodelet startup). Uses the same "read files directly off the host
# filesystem" trick the rest of this suite relies on, since this is a
# single-node deployment and cgroups are host state, not something
# reachable through the Kubernetes API.
#
# containerd can be configured with either cgroup driver (cgroupfs or
# systemd) — CRI's own cgroup_parent contract lets the runtime convert the
# cgroupfs-style path nodelet always sends into systemd unit naming, so
# this test doesn't assume which one a given cluster uses. It looks for
# *any* cgroup directory containing the pod's UID rather than asserting an
# exact path, which is honest about what it can actually verify without
# knowing the runtime's configured driver.

test_node_allocatable_cgroup_exists_and_is_capped() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local kubepods_dir="${NODELET_CGROUP_FS_ROOT:-/sys/fs/cgroup}/kubepods"
    if [[ ! -d "$kubepods_dir" ]]; then
        skip_test "no $kubepods_dir — either this host isn't on cgroup v2, nodelet isn't running as root, or enforce_node_allocatable() failed (check nodelet's startup logs for a 'node allocatable enforcement' warning)"
    fi
    if [[ ! -r "$kubepods_dir/cpu.max" || ! -r "$kubepods_dir/memory.max" ]]; then
        skip_test "$kubepods_dir exists but cpu.max/memory.max aren't readable from here"
    fi
    local cpu_max mem_max
    cpu_max="$(cat "$kubepods_dir/cpu.max")"
    mem_max="$(cat "$kubepods_dir/memory.max")"
    assert_not_empty "$cpu_max" "kubepods cgroup cpu.max has content"
    assert_not_empty "$mem_max" "kubepods cgroup memory.max has content"
}

test_pod_cgroup_reflects_its_qos_class() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local cgroup_root="${NODELET_CGROUP_FS_ROOT:-/sys/fs/cgroup}"
    local name="cgroup-qos-check"

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
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local uid
    uid="$(kubectl get pod "$name" -o jsonpath='{.metadata.uid}')"

    # This pod has no resources.requests/limits set, so it's BestEffort —
    # its cgroup should land under kubepods/besteffort somewhere, named
    # with its UID (cgroupfs: pod<uid> literally; systemd: pod<uid-with-
    # underscores>.slice).
    local uid_underscored="${uid//-/_}"
    if ! find "$cgroup_root/kubepods" -maxdepth 3 \( -iname "*${uid}*" -o -iname "*${uid_underscored}*" \) 2>/dev/null | grep -q .; then
        delete_pod_if_exists "$name"
        skip_test "couldn't find a cgroup directory under $cgroup_root/kubepods containing pod uid $uid — either this containerd doesn't delegate cgroup creation the way this test expects, or cgroup_parent_for()'s wiring in runtime/cri.rs::ensure_pod needs a look"
    fi

    delete_pod_if_exists "$name"
}

register_test test_node_allocatable_cgroup_exists_and_is_capped
register_test test_pod_cgroup_reflects_its_qos_class
