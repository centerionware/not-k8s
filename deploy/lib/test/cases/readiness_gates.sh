# lib/test/cases/readiness_gates.sh — spec.readinessGates (pods.rs's
# build_pod_status()): a pod's aggregate Ready condition must also wait on
# every readinessGates entry's named PodCondition being True, not just
# ContainersReady. This suite plays the role of the "external controller"
# itself by patching the pod's status.conditions directly via kubectl —
# genuinely automatable, no real controller needed.

test_pod_stays_not_ready_until_its_readiness_gate_condition_is_set() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="readiness-gate-check"
    local gate="www.example.com/feature-flag"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  readinessGates:
    - conditionType: "$gate"
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running

    # Container itself is ready (no readiness probe -> defaults to ready),
    # but the gate condition has never been set -> Ready must stay False.
    wait_until 15 "$name ContainersReady" bash -c "[[ \"\$(pod_condition_status '$name' ContainersReady)\" == 'True' ]]"
    assert_eq "$(pod_condition_status "$name" Ready)" "False" "Ready must be False while the readinessGates condition is unset — a missing gate condition counts as not-satisfied"

    # Play external controller: set the gate condition to False explicitly first.
    kubectl patch pod "$name" -n "$TEST_NAMESPACE" --subresource=status --type=merge -p \
        "{\"status\":{\"conditions\":[{\"type\":\"$gate\",\"status\":\"False\"}]}}" >/dev/null
    sleep 2
    assert_eq "$(pod_condition_status "$name" Ready)" "False" "Ready must stay False while the gate condition is explicitly False"

    # Now satisfy it.
    kubectl patch pod "$name" -n "$TEST_NAMESPACE" --subresource=status --type=merge -p \
        "{\"status\":{\"conditions\":[{\"type\":\"$gate\",\"status\":\"True\"}]}}" >/dev/null

    if ! try_wait_until 30 bash -c "[[ \"\$(pod_condition_status '$name' Ready)\" == 'True' ]]"; then
        delete_pod_if_exists "$name"
        die "Ready never flipped True after the readinessGates condition was set to True — check pods.rs::build_pod_status()'s gates_ready computation, or whether nodelet's own status write is clobbering the externally-set condition (JSON Merge Patch replaces the whole conditions array; build_pod_status() must copy foreign conditions forward)"
    fi

    # And the gate condition itself must have survived nodelet's own status
    # writes since it was set — proof foreign-condition carry-forward works,
    # not just that Ready happened to flip once and got frozen.
    assert_eq "$(pod_condition_status "$name" "$gate")" "True" "the externally-set gate condition must still be present after nodelet's subsequent status reconciles, not clobbered by the whole-array JSON Merge Patch"

    delete_pod_if_exists "$name"
}

register_test test_pod_stays_not_ready_until_its_readiness_gate_condition_is_set
