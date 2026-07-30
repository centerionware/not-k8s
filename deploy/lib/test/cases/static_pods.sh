# lib/test/cases/static_pods.sh — static pod manifest directory + mirror
# pod reconciliation (static_pods.rs). Needs nodelet actually running with
# NODELET_STATIC_POD_PATH set — off by default (matches real kubelet's
# optional staticPodPath), so this test needs to be told where that
# directory is via TEST_STATIC_POD_PATH; skips cleanly otherwise.

test_static_pod_creates_a_mirror_pod() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_STATIC_POD_PATH:-}" ]]; then
        skip_test "TEST_STATIC_POD_PATH not set — export it to the directory nodelet was started with NODELET_STATIC_POD_PATH=<that same dir> to exercise this"
    fi
    [[ -d "$TEST_STATIC_POD_PATH" ]] || die "TEST_STATIC_POD_PATH=$TEST_STATIC_POD_PATH is not a directory"

    local static_name="static-e2e-check"
    local manifest_path="$TEST_STATIC_POD_PATH/${static_name}.yaml"
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
