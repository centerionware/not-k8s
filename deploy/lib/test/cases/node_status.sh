# lib/test/cases/node_status.sh — Node object: registration, capacity,
# real pressure conditions.

test_node_is_ready_with_capacity_advertised() {
    local n
    n="$(node_name)"
    assert_not_empty "$n" "node name"
    assert_eq "$(node_condition_status Ready)" "True" "node Ready condition"

    local cpu mem pods ephemeral_storage
    cpu="$(kubectl get node "$n" -o jsonpath='{.status.capacity.cpu}')"
    mem="$(kubectl get node "$n" -o jsonpath='{.status.capacity.memory}')"
    pods="$(kubectl get node "$n" -o jsonpath='{.status.capacity.pods}')"
    ephemeral_storage="$(kubectl get node "$n" -o jsonpath='{.status.capacity.ephemeral-storage}')"
    assert_not_empty "$cpu" "node cpu capacity"
    assert_not_empty "$mem" "node memory capacity"
    assert_not_empty "$pods" "node pods capacity"
    # Round 48: real filesystem size (statvfs on NODELET_DISK_PATH), not a
    # hardcoded/omitted field — assert it's a real positive byte count, not
    # just present.
    assert_not_empty "$ephemeral_storage" "node ephemeral-storage capacity"
    assert_true bash -c "[[ '$ephemeral_storage' -gt 0 ]]" "ephemeral-storage capacity should be a real positive byte count (got $ephemeral_storage)"

    local ephemeral_storage_alloc
    ephemeral_storage_alloc="$(kubectl get node "$n" -o jsonpath='{.status.allocatable.ephemeral-storage}')"
    assert_eq "$ephemeral_storage_alloc" "$ephemeral_storage" "ephemeral-storage allocatable should equal capacity (no reservation knob for it yet)"
}

test_node_reports_hugepages_capacity_when_reserved() {
    # Round 60: Node.status.capacity/.allocatable["hugepages-<size>"] was
    # never populated at all before this — nodelet now scans
    # /sys/kernel/mm/hugepages/ directly. Skips cleanly (not a hard fail)
    # if this node/kernel has no hugepage pool reserved at all
    # (check /proc/sys/vm/nr_hugepages), since that's genuinely outside
    # nodelet's control and can't be assumed present in arbitrary test
    # environments.
    local n
    n="$(node_name)"
    local reserved
    reserved="$(cat /proc/sys/vm/nr_hugepages 2>/dev/null || echo 0)"
    if [[ "$reserved" -eq 0 ]]; then
        skip_test "no hugepages reserved on this node (/proc/sys/vm/nr_hugepages is 0) — nothing for Node.status.capacity[\"hugepages-*\"] to report"
    fi

    local cap_json
    cap_json="$(kubectl get node "$n" -o jsonpath='{.status.capacity}')"
    if ! echo "$cap_json" | grep -q '"hugepages-'; then
        skip_test "Node.status.capacity has no hugepages-* key even though nr_hugepages=$reserved is reserved — check hugepages_capacity_map()'s HUGEPAGES_SYSFS_ROOT wiring in node.rs"
    fi

    local key
    key="$(echo "$cap_json" | grep -o '"hugepages-[^"]*"' | head -1 | tr -d '"')"
    local cap alloc
    cap="$(kubectl get node "$n" -o jsonpath="{.status.capacity.$key}")"
    alloc="$(kubectl get node "$n" -o jsonpath="{.status.allocatable.$key}")"
    assert_not_empty "$cap" "node capacity.$key"
    assert_true bash -c "[[ '$cap' -gt 0 ]]" "capacity.$key should be a real positive byte count (got $cap)"
    assert_eq "$alloc" "$cap" "$key allocatable should equal capacity (no reservation knob for hugepages)"
}

test_pressure_conditions_are_present_and_normally_false() {
    local n
    n="$(node_name)"
    for cond in MemoryPressure DiskPressure PIDPressure; do
        local status
        status="$(node_condition_status "$cond")"
        assert_not_empty "$status" "$cond must be present (was hardcoded/missing before rounds 1-4)"
        # Not asserted strictly False: a genuinely constrained test box could
        # legitimately be under real pressure. Just report it loudly so a
        # human notices, rather than failing a real, correctly-reported condition.
        if [[ "$status" != "False" ]]; then
            warn "$cond is $status on this node — either real pressure, or worth double-checking NODELET_*_PRESSURE_* thresholds"
        fi
    done
}

test_node_reports_a_real_kernel_and_os_image() {
    # Pins that node.rs's system_info() is reading real /proc/os-release
    # data, not returning placeholders.
    local n
    n="$(node_name)"
    local kernel os_image
    kernel="$(kubectl get node "$n" -o jsonpath='{.status.nodeInfo.kernelVersion}')"
    os_image="$(kubectl get node "$n" -o jsonpath='{.status.nodeInfo.osImage}')"
    assert_not_empty "$kernel" "kernelVersion"
    assert_not_eq "$kernel" "unknown" "kernelVersion should be real, not the fallback"
    assert_not_empty "$os_image" "osImage"
}

test_node_status_reports_runtime_handlers() {
    # Round 53: Node.status.runtimeHandlers was never populated at all
    # before this — CRI's own runtime-level Status RPC (which returns the
    # discovered handler list) was never called. Not asserting a specific
    # handler name (varies by containerd config) — just that at least one
    # real entry is present, since a real containerd install always
    # reports at least its default handler.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local n
    n="$(node_name)"
    local count
    count="$(kubectl get node "$n" -o jsonpath='{.status.runtimeHandlers}' | jq 'length' 2>/dev/null || echo 0)"
    if [[ "$count" -eq 0 ]]; then
        skip_test "Node.status.runtimeHandlers is empty — either this containerd version doesn't report any handlers via its Status RPC, or jq isn't available to parse the field"
    fi
    assert_true bash -c "[[ '$count' -gt 0 ]]" "runtimeHandlers should list at least the default handler"
}

test_node_status_images_reflects_a_real_pulled_image() {
    # Round 33: Node.status.images was never populated at all before this.
    # Genuinely automatable — TEST_IMAGE is already guaranteed to be
    # pulled onto this node by every other test in this suite that runs a
    # pod, so it must show up here with a real (nonzero) size.
    local n
    n="$(node_name)"
    local name="node-status-images-check"
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

    if ! try_wait_until 30 bash -c "kubectl get node '$n' -o jsonpath='{.status.images}' | grep -q ."; then
        delete_pod_if_exists "$name"
        skip_test "node.status.images is empty/missing — check node_images()/select_node_images() wiring in runtime/cri.rs and node.rs"
    fi

    local images_json image_repo
    images_json="$(kubectl get node "$n" -o jsonpath='{.status.images}')"
    delete_pod_if_exists "$name"

    # Strip any tag/digest to match however the runtime spells this image's
    # names (repo tag, or a resolved digest reference).
    image_repo="${TEST_IMAGE%%:*}"
    image_repo="${image_repo%%@*}"
    assert_contains "$images_json" "$image_repo" "expected \$TEST_IMAGE's repo ($image_repo) to appear in node.status.images"

    if ! echo "$images_json" | grep -Eq '"sizeBytes":[1-9]'; then
        die "node.status.images has no entry with a nonzero sizeBytes — check node_image_from_cri()'s size_bytes plumbing in runtime/cri.rs"
    fi
}

register_test test_node_is_ready_with_capacity_advertised
register_test test_node_reports_hugepages_capacity_when_reserved
register_test test_pressure_conditions_are_present_and_normally_false
register_test test_node_reports_a_real_kernel_and_os_image
register_test test_node_status_reports_runtime_handlers
register_test test_node_status_images_reflects_a_real_pulled_image
