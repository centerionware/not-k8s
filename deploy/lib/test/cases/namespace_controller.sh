# namespace-controller's Group C finalizer path. This deliberately creates a
# disposable namespace with a real namespaced object, deletes the Namespace,
# and proves the object is removed before the Namespace can disappear. The
# test must be gated on this project's controller-manager replacement so a
# stock k3s controller-manager cannot make an incomplete translation look
# correct.

_namespace_controller_is_running() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_namespace_controller_k3s_manager_disabled() {
    local args=""
    if command -v systemctl >/dev/null 2>&1; then
        args="$(systemctl show k3s -p ExecStart --value 2>/dev/null || true)"
    fi
    [[ "$args" == *--disable-controller-manager* ]] && return 0
    ps -eo args= 2>/dev/null | grep -E '[k]3s( server)?' | grep -q -- '--disable-controller-manager'
}

_require_namespace_controller() {
    _namespace_controller_is_running \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller to exercise namespace finalization"
    _namespace_controller_k3s_manager_disabled \
        || skip_test "k3s's bundled controller-manager is still enabled; this test would not prove namespace-controller behavior"
}

namespace_gone() {
    ! kubectl get namespace "$1" >/dev/null 2>&1
}

_namespace_controller_cleanup_ns=""

_namespace_controller_cleanup() {
    [[ -n "$_namespace_controller_cleanup_ns" ]] || return 0
    kubectl delete namespace "$_namespace_controller_cleanup_ns" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    kubectl patch namespace "$_namespace_controller_cleanup_ns" --subresource=finalize --type=merge \
        -p '{"spec":{"finalizers":[]}}' >/dev/null 2>&1 || true
}

test_namespace_controller_deletes_contents_before_finalizing() {
    _require_namespace_controller

    local ns="namespace-controller-${BASHPID}-${RANDOM}"
    _namespace_controller_cleanup_ns="$ns"
    trap _namespace_controller_cleanup EXIT

    kubectl apply -f - <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: $ns
EOF

    kubectl apply --namespace="$ns" -f - <<EOF
apiVersion: v1
kind: ConfigMap
metadata:
  name: must-be-cleaned
data:
  proof: namespace-controller
EOF

    wait_until 30 "namespace contents exist before deletion" \
        bash -c "kubectl get configmap must-be-cleaned -n '$ns' >/dev/null 2>&1"

    kubectl delete namespace "$ns" --wait=false >/dev/null

    wait_until 120 "namespace-controller removes the namespaced object" \
        bash -c "! kubectl get configmap must-be-cleaned -n '$ns' >/dev/null 2>&1"
    wait_until 120 "namespace-controller removes the Namespace finalizer" \
        namespace_gone "$ns"

    trap - EXIT
    _namespace_controller_cleanup_ns=""
}

register_test test_namespace_controller_deletes_contents_before_finalizing
