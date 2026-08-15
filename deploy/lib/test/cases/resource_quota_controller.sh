# lib/test/cases/resource_quota_controller.sh — nodecontroller's Group D
# object-count slice: resourcequota-controller. No e2e coverage anywhere
# else in this suite — until nodecontroller existed this was entirely
# k3s's bundled controller-manager's job (see docs/CONTROLLER_MANAGER.md,
# Group D). Only status.used is under test here — quota *enforcement* is
# the apiserver's own ResourceQuota admission plugin, unrelated to which
# controller-manager runs, and already covered (or not) elsewhere.

_nodecontroller_is_running_rq() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_require_nodecontroller_rq() {
    _nodecontroller_is_running_rq \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller (which also disables k3s's own controller manager) to exercise these"
}

test_resourcequota_used_pods_tracks_actual_pod_count() {
    _require_nodecontroller_rq
    local quota="rq-test-quota" pod="rq-test-pod"

    apply_manifest <<EOF
apiVersion: v1
kind: ResourceQuota
metadata:
  name: $quota
spec:
  hard:
    pods: "100"
EOF
    trap 'delete_pod_if_exists "$pod"; kctl delete resourcequota "$quota" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  containers:
    - name: busybox
      image: busybox:latest
      command: ["sleep", "3600"]
EOF

    # Compares against the actual live pod count (not a hardcoded number)
    # so this doesn't assume exclusive ownership of the test namespace.
    wait_until 60 "ResourceQuota $quota's status.used.pods matches the actual pod count" \
        bash -c "[[ \"\$(kctl get resourcequota '$quota' -o jsonpath='{.status.used.pods}')\" == \"\$(kctl get pods --no-headers 2>/dev/null | wc -l | tr -d ' ')\" ]]"

    delete_pod_and_wait_gone "$pod"
    wait_until 60 "ResourceQuota $quota's status.used.pods drops after the pod is deleted" \
        bash -c "[[ \"\$(kctl get resourcequota '$quota' -o jsonpath='{.status.used.pods}')\" == \"\$(kctl get pods --no-headers 2>/dev/null | wc -l | tr -d ' ')\" ]]"

    trap - EXIT
    kctl delete resourcequota "$quota" --ignore-not-found >/dev/null 2>&1 || true
}

register_test test_resourcequota_used_pods_tracks_actual_pod_count
