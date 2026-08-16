# lib/test/cases/nftables_rebuild_debounce.sh — nodeproxy's REBUILD_DEBOUNCE
# (svc.rs), added investigating github.com/centerionware/not-k8s/issues/30:
# a burst of Service/EndpointSlice events (e.g. a Deployment scaling up N
# pods) used to rebuild the whole nftables table once per individual event,
# in rapid succession — the leading suspect for real TCP resets observed on
# long-lived pod-to-apiserver connections held open across one of those
# rebuilds (CoreDNS's own watch to the apiserver, concretely). This proves
# a long-lived, real in-cluster HTTPS connection survives a real burst of
# unrelated Service churn, from inside a pod (the same path CoreDNS/any
# in-cluster client uses — nodeproxy's ClusterIP DNAT), not just that
# routing eventually converges (service_proxy.sh already covers that).

test_a_long_lived_watch_survives_a_service_churn_burst() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local pod="debounce-watcher"

    apply_manifest <<PODEOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  serviceAccountName: default
  containers:
    - name: watcher
      image: $TEST_IMAGE
      command: ["sh", "-c", "apk add --no-cache curl >/tmp/apk.log 2>&1; TOKEN=\$(cat /var/run/secrets/kubernetes.io/serviceaccount/token); CACERT=/var/run/secrets/kubernetes.io/serviceaccount/ca.crt; curl -sS -N --connect-timeout 5 --max-time 90 --cacert \$CACERT -H \"Authorization: Bearer \$TOKEN\" \"https://\$KUBERNETES_SERVICE_HOST:\$KUBERNETES_SERVICE_PORT/api/v1/namespaces/$TEST_NAMESPACE/pods?watch=true&timeoutSeconds=85\" > /tmp/watch.out 2>/tmp/watch.err; echo WATCH_EXIT=\$?; sleep 3600"]
PODEOF
    trap 'kubectl delete pod "$pod" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true; for i in $(seq 1 25); do kubectl delete svc "churn-svc-$i" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true; done' EXIT

    wait_until 30 "$pod Running" pod_is_phase "$pod" Running
    sleep 5

    # The actual burst: 25 Services created back-to-back, no delay between
    # them — exactly the shape of event storm a Deployment scale-up
    # produces via its EndpointSlice updates, deliberately compressed into
    # a couple of seconds so nodeproxy's own table-rebuild churns hard
    # while the watch above is idle (past its initial response, waiting on
    # the next event — the connection state this bug actually hits).
    for i in $(seq 1 25); do
        apply_manifest <<SVCEOF
apiVersion: v1
kind: Service
metadata:
  name: churn-svc-$i
spec:
  selector:
    app: churn-svc-$i-nonexistent
  ports:
    - port: 80
      targetPort: 80
SVCEOF
    done

    # Give the watch time to either keep streaming or die; timeoutSeconds=85
    # on the watch itself means it should still be alive well before then if
    # healthy.
    sleep 20

    echo "=== watcher pod status ==="
    kubectl get pod "$pod" -n "$TEST_NAMESPACE" -o wide
    local exec_out
    exec_out="$(kubectl exec "$pod" -n "$TEST_NAMESPACE" -- sh -c 'cat /tmp/watch.err 2>/dev/null; echo ---; wc -l < /tmp/watch.out 2>/dev/null' 2>&1)"
    echo "$exec_out"
    assert_not_contains "$exec_out" "connection reset by peer" \
        "a burst of Service churn must not reset a long-lived pod-to-apiserver connection"

    for i in $(seq 1 25); do
        kubectl delete svc "churn-svc-$i" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true
    done
    trap - EXIT
    kubectl delete pod "$pod" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true
}

register_test test_a_long_lived_watch_survives_a_service_churn_burst
