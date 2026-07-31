# lib/test/cases/gc.sh — garbage collection. Orphaned-sandbox GC and
# node-pressure eviction genuinely need to stop nodelet or exhaust real
# resources to trigger — this suite won't do either to a service/host you
# may be relying on, so those are documented manual procedures (skipped by
# default) rather than automated tests. Image GC (round 70) is now real
# kubelet's own watermark policy too — an unreferenced image is left alone
# entirely unless disk usage crosses NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT
# (default 85%), so exercising *actual* removal joins the manual-procedure
# group above; what's still safely automatable is proving the negative —
# a freshly-unreferenced image is NOT swept away just because it's unused,
# which is the whole point of round 70's change from the old unconditional
# sweep.

test_pod_teardown_actually_removes_the_sandbox() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! command -v ctr >/dev/null 2>&1; then
        skip_test "no 'ctr' (containerd CLI) on PATH to verify sandbox removal"
    fi
    local name="teardown-check"
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
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local container_id
    container_id="$(pod_field "$name" '{.status.containerStatuses[0].containerID}')"
    container_id="${container_id#containerd://}"
    assert_not_empty "$container_id" "container ID before deletion"

    kctl delete pod "$name" --wait=false >/dev/null
    wait_until 30 "$name gone from apiserver" pod_gone "$name"
    # Give nodelet a moment to actually process the delete watch event and
    # tear the sandbox down (this races the apiserver delete slightly).
    try_wait_until 20 bash -c "! ctr -n k8s.io containers ls -q 2>/dev/null | grep -qx '$container_id'" \
        || die "container $container_id is still present in containerd after its pod was deleted"
}

test_orphaned_sandbox_gc_manual_procedure() {
    skip_test "needs stopping nodelet, deleting a pod from the apiserver while it's down, then restarting and watching gc_loop clean up the now-orphaned sandbox — not something this suite automates against a live service. Manual steps: (1) apply a test pod and wait Running, (2) sudo systemctl stop nodelet, (3) kubectl delete pod <name> --wait=false, (4) sudo systemctl start nodelet, (5) within NODELET_GC_INTERVAL_SECS confirm 'ctr -n k8s.io containers ls' no longer shows it."
}

test_unreferenced_image_is_not_removed_below_the_watermark() {
    # Round 70: image GC used to sweep every unreferenced image on every
    # cycle regardless of disk pressure — that unconditional-removal
    # behavior is exactly what this round replaced. On any reasonable
    # test node (well under NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT,
    # default 85%), a freshly-unreferenced image must survive a full GC
    # cycle untouched.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! command -v ctr >/dev/null 2>&1; then
        skip_test "no 'ctr' (containerd CLI) on PATH to verify image state"
    fi
    local image="busybox:1.36.1" name="image-gc-below-watermark-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $image
      command: ["sleep", "60"]
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    assert_true bash -c "ctr -n k8s.io images ls -q | grep -q '$image'"
    kctl delete pod "$name" --wait=false >/dev/null
    wait_until 30 "$name gone" pod_gone "$name"

    log "    waiting through at least one NODELET_GC_INTERVAL_SECS cycle (default 300s) to confirm $image survives it..."
    sleep 60
    assert_true bash -c "ctr -n k8s.io images ls -q | grep -q '$image'" \
        "an unreferenced image below the image-GC high watermark must NOT be removed — if this fails, either disk usage on this node genuinely is at/above NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT (check 'df' on NODELET_DISK_PATH), or should_start_image_gc()'s gating broke"
}

test_image_gc_watermark_removal_manual_procedure() {
    skip_test "exercising *actual* image removal under real disk pressure needs either genuinely filling NODELET_DISK_PATH's filesystem or restarting nodelet with an artificially-low NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT — neither is something this suite does to a live node automatically (same reasoning as the orphaned-sandbox/eviction manual procedures above). Manual procedure: (1) note current disk usage via 'df' on NODELET_DISK_PATH, (2) restart nodelet with NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT set at or below current usage and NODELET_IMAGE_GC_MIN_AGE_SECS set low (e.g. 5) for a fast test, (3) apply and delete a pod using a distinct scratch image tag (same pattern as test_unreferenced_image_is_not_removed_below_the_watermark), (4) within NODELET_GC_INTERVAL_SECS confirm 'ctr -n k8s.io images ls' no longer shows it, (5) confirm images NOT unreferenced (or referenced by other running pods) are left alone, (6) restore normal thresholds and restart nodelet."
}

register_test test_pod_teardown_actually_removes_the_sandbox
register_test test_orphaned_sandbox_gc_manual_procedure
register_test test_unreferenced_image_is_not_removed_below_the_watermark
register_test test_image_gc_watermark_removal_manual_procedure
