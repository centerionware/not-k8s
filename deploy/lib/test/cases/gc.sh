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
#
# Round 123: every `ctr` call below runs under `sudo` — found live on CI
# that the e2e suite's own step doesn't run as root (unlike the earlier
# build/install steps, which do), and containerd's CRI socket is
# root-only. Without sudo, `ctr` fails with a permission-denied error on
# stderr and prints nothing on stdout — which silently made the two
# absence checks below (`! ctr ... | grep -qx ...`, i.e. "is this
# container gone") pass for the wrong reason (no output at all still
# satisfies "doesn't contain this ID"), while the one presence check
# (image GC's `ctr ... | grep -q ...`) correctly failed instead of
# passing, since it needs real output to match against.

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
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local container_id
    container_id="$(pod_field "$name" '{.status.containerStatuses[0].containerID}')"
    container_id="${container_id#containerd://}"
    assert_not_empty "$container_id" "container ID before deletion"

    kctl delete pod "$name" --wait=false >/dev/null
    wait_until 30 "$name gone from apiserver" pod_gone "$name"
    # Give nodelet a moment to actually process the delete watch event and
    # tear the sandbox down (this races the apiserver delete slightly).
    try_wait_until 20 bash -c "! sudo ctr -n k8s.io containers ls -q 2>/dev/null | grep -qx '$container_id'" \
        || die "container $container_id is still present in containerd after its pod was deleted"
}

test_pod_with_a_finalizer_tears_down_but_stays_until_the_finalizer_is_removed() {
    # Round 103 gave teardown() a real Api::<Pod>::delete() call so a
    # deleted pod's object actually leaves the apiserver instead of
    # parking in Terminating forever (see docs/E2E_FINDINGS.md finding
    # #1) — but nothing anywhere (nodelet's own code or this suite) had
    # ever exercised a finalizer-blocked pod through that path. Proves
    # the two things a finalizer is supposed to guarantee still hold:
    # container teardown doesn't wait on it (finalizers are an apiserver/
    # object-removal concept, unrelated to kubelet stopping containers),
    # and the delete() call doesn't error or infinite-loop against an
    # object it can't actually finish deleting — it just stays
    # Terminating, correctly, until the finalizer is gone.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! command -v ctr >/dev/null 2>&1; then
        skip_test "no 'ctr' (containerd CLI) on PATH to verify sandbox removal"
    fi
    local name="finalizer-check"
    local finalizer="e2e.not-k8s.dev/test-finalizer"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
  finalizers: ["$finalizer"]
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    # Cleanup runs even if an assertion below dies mid-test — a leftover
    # finalizer would otherwise wedge this pod (and this test's namespace
    # deletion) forever, unlike every other test's plain delete_pod_if_exists.
    # EXIT, not RETURN: die() (what assert_*/the die calls below use) exits
    # the subshell run_test() runs each test in outright rather than
    # returning from this function normally, and a RETURN trap does not
    # fire on exit — confirmed the hard way, live: the first version of
    # this test used RETURN, failed an assertion, and left its pod's
    # finalizer in place, wedging the whole test namespace's deletion.
    finalizer_check_cleanup() {
        kctl patch pod "$name" --type=merge -p '{"metadata":{"finalizers":[]}}' >/dev/null 2>&1 || true
        delete_pod_if_exists "$name"
    }
    trap finalizer_check_cleanup EXIT

    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local container_id
    container_id="$(pod_field "$name" '{.status.containerStatuses[0].containerID}')"
    container_id="${container_id#containerd://}"
    assert_not_empty "$container_id" "container ID before deletion"

    kctl delete pod "$name" --wait=false >/dev/null

    # Container teardown must happen regardless of the finalizer. 40s, not
    # the 20s the finalizer-free version of this check uses above — a
    # finalizer-blocked pod never gets an Event::Delete (the object never
    # actually leaves the apiserver), so reconcile() only has the
    # Modified/Apply event from deletionTimestamp being set to react to;
    # confirmed live this reliably still finishes well under a minute, just
    # not as fast as the plain-delete path, which gets both that event and
    # a fast follow-up Delete event once the object is actually gone.
    try_wait_until 40 bash -c "! sudo ctr -n k8s.io containers ls -q 2>/dev/null | grep -qx '$container_id'" \
        || die "container $container_id is still present in containerd after pod delete, even though the pod has a finalizer blocking apiserver removal — teardown() must not wait on finalizers"

    # The pod object itself must survive — deletionTimestamp set, the
    # finalizer we put there still listed, and NOT gone from the apiserver
    # (teardown()'s delete() call must not error its way around the
    # finalizer, and must not spin retrying it either).
    sleep 3
    pod_exists "$name" || die "pod $name disappeared from the apiserver despite an unremoved finalizer — a finalizer must block actual object removal"
    local deletion_ts finalizers
    deletion_ts="$(pod_field "$name" '{.metadata.deletionTimestamp}')"
    finalizers="$(pod_field "$name" '{.metadata.finalizers}')"
    assert_not_empty "$deletion_ts" "deletionTimestamp should be set"
    assert_contains "$finalizers" "$finalizer" "the finalizer should still be listed"

    # Removing the finalizer should let the object actually go away, same
    # as it would for any other controller's finalizer. Purely an
    # apiserver-side mechanism once every finalizer is gone — nodelet
    # itself has nothing left to do here — but found live (round 123, on
    # a CI runner) that 20s isn't always enough headroom for the
    # apiserver to actually process the removal under real load (this
    # test runs right after two churn-heavy eviction tests); 30s matches
    # this file's own earlier acknowledgment that finalizer-blocked
    # teardown is reliably slower than the plain-delete path, not
    # instant.
    kctl patch pod "$name" --type=merge -p '{"metadata":{"finalizers":[]}}' >/dev/null
    wait_until 30 "$name gone once its finalizer is removed" pod_gone "$name"
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
    assert_true bash -c "sudo ctr -n k8s.io images ls -q | grep -q '$image'"
    kctl delete pod "$name" --wait=false >/dev/null
    wait_until 30 "$name gone" pod_gone "$name"

    log "    waiting through at least one NODELET_GC_INTERVAL_SECS cycle (default 300s) to confirm $image survives it..."
    sleep 60
    assert_true bash -c "sudo ctr -n k8s.io images ls -q | grep -q '$image'" \
        "an unreferenced image below the image-GC high watermark must NOT be removed — if this fails, either disk usage on this node genuinely is at/above NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT (check 'df' on NODELET_DISK_PATH), or should_start_image_gc()'s gating broke"
}

test_image_gc_watermark_removal_manual_procedure() {
    skip_test "exercising *actual* image removal under real disk pressure needs either genuinely filling NODELET_DISK_PATH's filesystem or restarting nodelet with an artificially-low NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT — neither is something this suite does to a live node automatically (same reasoning as the orphaned-sandbox/eviction manual procedures above). Manual procedure: (1) note current disk usage via 'df' on NODELET_DISK_PATH, (2) restart nodelet with NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT set at or below current usage and NODELET_IMAGE_GC_MIN_AGE_SECS set low (e.g. 5) for a fast test, (3) apply and delete a pod using a distinct scratch image tag (same pattern as test_unreferenced_image_is_not_removed_below_the_watermark), (4) within NODELET_GC_INTERVAL_SECS confirm 'ctr -n k8s.io images ls' no longer shows it, (5) confirm images NOT unreferenced (or referenced by other running pods) are left alone, (6) restore normal thresholds and restart nodelet."
}

register_test test_pod_teardown_actually_removes_the_sandbox
register_test test_pod_with_a_finalizer_tears_down_but_stays_until_the_finalizer_is_removed
register_test test_orphaned_sandbox_gc_manual_procedure
register_test test_unreferenced_image_is_not_removed_below_the_watermark
register_test test_image_gc_watermark_removal_manual_procedure
