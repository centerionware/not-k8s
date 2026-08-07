# lib/test/cases/static_pods.sh — static pod manifest directory + mirror
# pod reconciliation (static_pods.rs). Needs nodelet actually running with
# NODELET_STATIC_POD_PATH set — off by default (matches real kubelet's
# optional staticPodPath). Round 123: previously a "manual spot-check
# only" test (this harness had no way to inject nodelet startup env vars
# per test) — now uses nodelet_restart_with_env (nodelet_env.sh) to
# actually restart nodelet with a real static-pod directory for the
# duration of this one test, then restart it back to normal.

test_static_pod_creates_a_mirror_pod() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to restart nodelet with a different NODELET_STATIC_POD_PATH"; fi

    local static_pod_path
    static_pod_path="$(mktemp -d /tmp/nodelet-static-pods-test.XXXXXX)"
    # EXIT, not RETURN — same reasoning gc.sh's finalizer test doc comment
    # gives: a die()/assert failure exits this test's own subshell outright
    # rather than returning normally, and a leftover nodelet_env override
    # would otherwise silently corrupt every test that runs after this one.
    static_pod_test_cleanup() {
        nodelet_restore_env
        rm -rf "${static_pod_path:-}"
    }
    trap static_pod_test_cleanup EXIT

    nodelet_restart_with_env "NODELET_STATIC_POD_PATH=$static_pod_path"

    local static_name="static-e2e-check"
    local manifest_path="$static_pod_path/${static_name}.yaml"
    local n
    n="$(node_name)"
    local mirror_name="${static_name}-${n}"

    cat > "$manifest_path" <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $static_name
  namespace: $TEST_NAMESPACE
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF

    wait_until 60 "mirror pod $mirror_name Running" pod_is_phase "$mirror_name" Running
    local mirror_annotation
    mirror_annotation="$(kctl get pod "$mirror_name" -o jsonpath='{.metadata.annotations.kubernetes\.io/config\.mirror}')"
    assert_not_empty "$mirror_annotation" "kubernetes.io/config.mirror annotation on the mirror pod"

    rm -f "$manifest_path"
    wait_until 60 "mirror pod $mirror_name removed after manifest deletion" pod_gone "$mirror_name"
}

register_test test_static_pod_creates_a_mirror_pod
