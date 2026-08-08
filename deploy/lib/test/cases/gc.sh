# lib/test/cases/gc.sh — garbage collection. Orphaned-sandbox GC needs
# stopping nodelet, and image-GC watermark removal (round 70's real
# kubelet-style policy — an unreferenced image is left alone entirely
# unless disk usage crosses NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT,
# default 85%) needs a threshold this node will never naturally cross —
# round 123 automated both anyway via nodelet_restart_with_env
# (nodelet_env.sh: stop/reconfigure/restart nodelet for the one test that
# needs it, restore defaults after). test_unreferenced_image_is_not_removed_
# below_the_watermark still separately proves the negative case (a
# freshly-unreferenced image is NOT swept just because it's unused) against
# nodelet's own normal, unmodified startup config.
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
    wait_until 90 "$name Running" pod_is_phase "$name" Running
    local container_id
    container_id="$(pod_field "$name" '{.status.containerStatuses[0].containerID}')"
    container_id="${container_id#containerd://}"
    assert_not_empty "$container_id" "container ID before deletion"

    kctl delete pod "$name" --wait=false >/dev/null
    # Round 124 (found live in CI, full-suite runs only): 30s wasn't
    # always enough for real pod deletion to complete when the whole
    # unfiltered suite is contending for the same node/containerd —
    # confirmed live: this timed out right after two unrelated eviction
    # tests' own pod churn ("failed to find sandbox id ... not found"
    # retries) immediately before it. Same reasoning csi_pvc.sh's own
    # post-delete pod_gone wait already documents.
    wait_until 120 "$name gone from apiserver" pod_gone "$name"
    # Give nodelet a moment to actually process the delete watch event and
    # tear the sandbox down (this races the apiserver delete slightly).
    try_wait_until 40 bash -c "! sudo ctr -n k8s.io containers ls -q 2>/dev/null | grep -qx '$container_id'" \
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

    wait_until 90 "$name Running" pod_is_phase "$name" Running
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
    try_wait_until 60 bash -c "! sudo ctr -n k8s.io containers ls -q 2>/dev/null | grep -qx '$container_id'" \
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
    wait_until 90 "$name gone once its finalizer is removed" pod_gone "$name"
}

test_orphaned_sandbox_gc_reaps_a_pod_deleted_while_nodelet_is_down() {
    # Round 123: previously manual-only purely because this harness had
    # no way to stop/start nodelet with a short NODELET_GC_INTERVAL_SECS
    # (default 300s -- far too slow for a test window) — now uses
    # nodelet_restart_with_env (nodelet_env.sh) for that, and systemctl
    # stop/start directly (the harness already depends on systemd for
    # nodelet_restart_supported).
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! command -v ctr >/dev/null 2>&1; then
        skip_test "no 'ctr' (containerd CLI) on PATH to verify sandbox removal"
    fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to stop/start nodelet and set a short NODELET_GC_INTERVAL_SECS"; fi

    orphaned_gc_test_cleanup() { nodelet_restore_env; }
    trap orphaned_gc_test_cleanup EXIT
    nodelet_restart_with_env "NODELET_GC_INTERVAL_SECS=10"

    local name="orphaned-sandbox-gc-check"
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
    wait_until 90 "$name Running" pod_is_phase "$name" Running
    local container_id
    container_id="$(pod_field "$name" '{.status.containerStatuses[0].containerID}')"
    container_id="${container_id#containerd://}"
    assert_not_empty "$container_id" "container ID before nodelet goes down"

    sudo systemctl stop nodelet.service
    # Round 124 (found live in CI): a plain graceful delete (no
    # --grace-period=0 --force) can NEVER actually finish while nodelet
    # is down -- that's real, correct Kubernetes behavior, not a nodelet
    # bug: the API server sets deletionTimestamp and waits for the
    # owning kubelet's own final acknowledgment DELETE to actually
    # remove the object from etcd (the whole reason `--force
    # --grace-period=0` exists as an escape hatch for exactly "the node
    # is unreachable/down"). The previous version of this test waited up
    # to 180s for the plain-delete case to somehow resolve on its own
    # with nodelet stopped -- it never could, no matter how generous the
    # timeout, because nothing else was ever going to finish it. Forcing
    # the delete is also the *more correct* simulation of this test's
    # own scenario (a pod that disappears from the apiserver while its
    # node is unreachable, leaving a genuinely orphaned sandbox behind)
    # -- a plain graceful delete instead just leaves a normal
    # deletionTimestamp'd pod nodelet's ordinary reconcile handles once
    # it's back, never actually exercising the orphan-GC path this test
    # means to check at all.
    kctl delete pod "$name" --wait=false --grace-period=0 --force >/dev/null
    wait_until 30 "$name gone from apiserver" pod_gone "$name"
    sudo systemctl start nodelet.service
    _nodelet_wait_ready "node Ready after restarting nodelet post-stop"

    try_wait_until 120 bash -c "! sudo ctr -n k8s.io containers ls -q 2>/dev/null | grep -qx '$container_id'" \
        || die "orphaned sandbox for $container_id was never GC'd within a couple of NODELET_GC_INTERVAL_SECS=10 cycles after restarting nodelet"
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
    wait_until 90 "$name Running" pod_is_phase "$name" Running
    assert_true bash -c "sudo ctr -n k8s.io images ls -q | grep -q '$image'"
    kctl delete pod "$name" --wait=false >/dev/null
    # Round 124 (found live in CI, full-suite runs only): 30s wasn't always
    # enough for real pod teardown to complete when the whole unfiltered
    # suite is contending for the same node/containerd — same reasoning
    # csi_pvc.sh's own post-delete pod_gone wait already documents.
    wait_until 120 "$name gone" pod_gone "$name"

    log "    waiting through at least one NODELET_GC_INTERVAL_SECS cycle (default 300s) to confirm $image survives it..."
    sleep 60
    assert_true bash -c "sudo ctr -n k8s.io images ls -q | grep -q '$image'" \
        "an unreferenced image below the image-GC high watermark must NOT be removed — if this fails, either disk usage on this node genuinely is at/above NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT (check 'df' on NODELET_DISK_PATH), or should_start_image_gc()'s gating broke"
}

test_image_gc_removes_unreferenced_images_above_the_watermark() {
    # Round 123: previously manual-only purely because it needs an
    # artificially-low NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT (this
    # node's REAL disk usage is never going to naturally cross 85%) and a
    # short NODELET_IMAGE_GC_MIN_AGE_SECS/NODELET_GC_INTERVAL_SECS — all
    # three now set via nodelet_restart_with_env (nodelet_env.sh). Uses a
    # distinct image tag from test_unreferenced_image_is_not_removed_below_
    # the_watermark (the negative-case sibling test) so the two don't
    # interfere with each other's unreferenced-since tracking.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! command -v ctr >/dev/null 2>&1; then
        skip_test "no 'ctr' (containerd CLI) on PATH to verify image state"
    fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to restart nodelet with a low image-GC watermark/interval"; fi

    local current_usage watermark
    current_usage="$(df --output=pcent "${NODELET_DISK_PATH:-/}" 2>/dev/null | tail -1 | tr -dc '0-9')"
    if [[ -z "$current_usage" || "$current_usage" -ge 99 ]]; then
        skip_test "couldn't read a usable disk usage percentage from df to pick a watermark below it"
    fi
    # Round 124 (found live in CI, twice): should_start_image_gc() gates
    # on `usage_percent >= high_threshold_percent`. A first attempt at
    # this fix set the threshold to (this df snapshot - 3) and it *still*
    # never triggered -- because nodelet's own NODELET_DISK_PATH default
    # is /var/lib/nodelet (config.rs), not the `/` this test's own `df`
    # fallback measures; nodelet computes its own usage_percent from a
    # raw statvfs call, not `df`'s own display logic (which factors in
    # the filesystem's root-reserved blocks and rounds differently) --
    # two genuinely independent measurements of not-quite-the-same-thing,
    # never guaranteed to land within any small fixed margin of each
    # other. Don't try to approximate nodelet's own number at all: just
    # pick a threshold comfortably low enough (10%) that it's true
    # regardless of which measurement methodology or mount is used --
    # any real CI runner's actual disk usage is nowhere near that low.
    watermark=10
    if [[ "$current_usage" -lt "$watermark" ]]; then
        skip_test "this node's disk usage ($current_usage%) is below even a deliberately low 10% watermark -- can't validate GC triggers above a threshold nothing on this node exceeds"
    fi

    # Round 124 (found live in CI, third bug in this same test):
    # triggering the sweep (HIGH_THRESHOLD) isn't enough on its own --
    # images_to_reclaim_space() *also* stops (before removing anything
    # at all) once simulated usage is already <= LOW_THRESHOLD, and its
    # default is 80%. Real usage here (~40%) is already under that
    # default, so the sweep triggered but immediately decided there was
    # nothing to do, every time, regardless of the high watermark fix.
    # Needs to be pushed down below real usage too, for the same "any
    # real CI runner's usage is nowhere near this low" reasoning as the
    # high threshold above.
    image_gc_test_cleanup() { nodelet_restore_env; }
    trap image_gc_test_cleanup EXIT
    nodelet_restart_with_env \
        "NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT=$watermark" \
        "NODELET_IMAGE_GC_LOW_THRESHOLD_PERCENT=5" \
        "NODELET_IMAGE_GC_MIN_AGE_SECS=1" \
        "NODELET_GC_INTERVAL_SECS=10"

    local image="busybox:1.33.1" name="image-gc-watermark-check"
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
    wait_until 90 "$name Running" pod_is_phase "$name" Running
    assert_true bash -c "sudo ctr -n k8s.io images ls -q | grep -q '$image'"
    kctl delete pod "$name" --wait=false >/dev/null
    wait_until 90 "$name gone" pod_gone "$name"

    # Round 124 (found live in CI, full-suite tail-end contention only):
    # 60s wasn't always enough for a real GC cycle to actually sweep the
    # image once this test lands at the tail of a long, otherwise-
    # unfiltered shard.
    try_wait_until 120 bash -c "! sudo ctr -n k8s.io images ls -q 2>/dev/null | grep -q '$image'" \
        || die "unreferenced image $image was never GC'd despite the watermark being set at/below this node's real disk usage ($current_usage%) and past NODELET_IMAGE_GC_MIN_AGE_SECS — check should_start_image_gc()'s gating"
}

register_test test_pod_teardown_actually_removes_the_sandbox
register_test test_pod_with_a_finalizer_tears_down_but_stays_until_the_finalizer_is_removed
register_test test_orphaned_sandbox_gc_reaps_a_pod_deleted_while_nodelet_is_down
register_test test_unreferenced_image_is_not_removed_below_the_watermark
register_test test_image_gc_removes_unreferenced_images_above_the_watermark
