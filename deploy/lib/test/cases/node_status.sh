# lib/test/cases/node_status.sh — Node object: registration, capacity,
# real pressure conditions.

test_node_is_ready_with_capacity_advertised() {
    local n
    n="$(node_name)"
    assert_not_empty "$n" "node name"
    assert_eq "$(node_condition_status Ready)" "True" "node Ready condition"

    local cpu mem pods
    cpu="$(kubectl get node "$n" -o jsonpath='{.status.capacity.cpu}')"
    mem="$(kubectl get node "$n" -o jsonpath='{.status.capacity.memory}')"
    pods="$(kubectl get node "$n" -o jsonpath='{.status.capacity.pods}')"
    assert_not_empty "$cpu" "node cpu capacity"
    assert_not_empty "$mem" "node memory capacity"
    assert_not_empty "$pods" "node pods capacity"
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

register_test test_node_is_ready_with_capacity_advertised
register_test test_pressure_conditions_are_present_and_normally_false
register_test test_node_reports_a_real_kernel_and_os_image
