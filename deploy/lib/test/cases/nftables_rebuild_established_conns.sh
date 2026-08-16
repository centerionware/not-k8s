# lib/test/cases/nftables_rebuild_established_conns.sh — nodeproxy's
# conntrack preservation across a ruleset replacement. NAT rules select a
# connection on its first packet; this test verifies the kernel keeps the
# resulting NAT binding for an already-established flow, rather than relying
# on an ineffective established-state rule in the NAT chain.
# github.com/centerionware/not-k8s/issues/30: a burst of Service/
# EndpointSlice events (e.g. a Deployment scaling up N pods) rebuilds the
# whole nftables table once per event — real TCP resets were observed on
# long-lived pod-to-apiserver connections held open across one of those
# rebuilds (CoreDNS's own watch to the apiserver, concretely), because an
# already-established connection's conntrack NAT state wasn't preserved
# across the wholesale chain replace. This proves a long-lived,
# real in-cluster HTTPS connection survives a real burst of unrelated
# Service churn, from inside a pod (the same path CoreDNS/any in-cluster
# client uses — nodeproxy's ClusterIP DNAT), not just that routing
# eventually converges (service_proxy.sh already covers that).

test_a_long_lived_watch_survives_a_service_churn_burst() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local pod="established-conn-watcher"

    # The `default` ServiceAccount gets no RBAC beyond its own token by
    # design — real Kubernetes RBAC, not something this test should widen
    # cluster-wide. A dedicated Role+RoleBinding scoped to this one
    # namespace's pods, same shape CoreDNS's own ClusterRole grants it for
    # the resources it actually watches. Found live in CI: without this the
    # watch below gets a real, correct 403 Forbidden that looks identical
    # (0 bytes, clean exit) to the connection never being established at
    # all — see the try_wait_until diagnostic branch below.
    apply_manifest <<RBACEOF
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: established-conn-watcher-pods
  namespace: $TEST_NAMESPACE
rules:
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["get", "list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: established-conn-watcher-pods
  namespace: $TEST_NAMESPACE
subjects:
  - kind: ServiceAccount
    name: default
    namespace: $TEST_NAMESPACE
roleRef:
  kind: Role
  name: established-conn-watcher-pods
  apiGroup: rbac.authorization.k8s.io
RBACEOF

    # A dedicated curl-bundling image, not $TEST_IMAGE + a runtime `apk add`:
    # installing curl at container-start time depends on live internet
    # access to Alpine's CDN *from the pod network*, which is unreliable in
    # CI (confirmed live: "temporary error (try again later)" / DNS
    # resolution flakiness) and is a test-infra concern this test has no
    # business depending on. curlimages/curl is Alpine-based with curl
    # (and a real shell) already in the image, so the only network access
    # this test needs at pod-start is the same image pull every other test
    # here already relies on working.
    apply_manifest <<PODEOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  serviceAccountName: default
  containers:
    - name: watcher
      image: curlimages/curl:8.10.1
      command: ["sh", "-c", "TOKEN=\$(cat /var/run/secrets/kubernetes.io/serviceaccount/token); CACERT=/var/run/secrets/kubernetes.io/serviceaccount/ca.crt; echo \"host=\$KUBERNETES_SERVICE_HOST port=\$KUBERNETES_SERVICE_PORT\" > /tmp/watch.err; curl -v -sS -N --connect-timeout 5 --max-time 90 --cacert \$CACERT -H \"Authorization: Bearer \$TOKEN\" \"https://\$KUBERNETES_SERVICE_HOST:\$KUBERNETES_SERVICE_PORT/api/v1/namespaces/$TEST_NAMESPACE/pods?watch=true&timeoutSeconds=85\" > /tmp/watch.out 2>>/tmp/watch.err; echo WATCH_EXIT=\$? >> /tmp/watch.err; sleep 3600"]
PODEOF
    trap 'kubectl delete pod "$pod" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true; kubectl delete role established-conn-watcher-pods -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true; kubectl delete rolebinding established-conn-watcher-pods -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true; for i in $(seq 1 25); do kubectl delete svc "churn-svc-$i" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true; done' EXIT

    wait_until 30 "$pod Running" pod_is_phase "$pod" Running

    # Wait for the watch to actually be alive and past its initial response
    # (real bytes received, curl still running) before starting the churn —
    # a blind fixed sleep here previously raced curl's own startup (TLS
    # handshake) and produced a false failure with 0 bytes received before
    # the burst ever ran. This wait is the thing the test is actually about:
    # an idle, already-established watch connection.
    watch_started() {
        local bytes pids
        bytes="$(kubectl exec "$pod" -n "$TEST_NAMESPACE" -- sh -c 'wc -c < /tmp/watch.out 2>/dev/null' 2>/dev/null)"
        pids="$(kubectl exec "$pod" -n "$TEST_NAMESPACE" -- sh -c 'pgrep curl | wc -l' 2>/dev/null)"
        [ "${bytes:-0}" -gt 0 ] && [ "${pids:-0}" -gt 0 ]
    }
    if ! try_wait_until 45 watch_started; then
        echo "=== watch never established; watch.out + watch.err ==="
        kubectl exec "$pod" -n "$TEST_NAMESPACE" -- sh -c 'echo "--watch.out--"; cat /tmp/watch.out 2>/dev/null; echo "--watch.err--"; cat /tmp/watch.err 2>/dev/null' 2>&1
        die "watch connection never got past its initial response before the churn burst — test infra issue, not the regression under test"
    fi

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
    assert_true bash -c "kubectl get pod '$pod' -n '$TEST_NAMESPACE' -o jsonpath='{.status.phase}' | grep -q Running"

    # Print watch.err *before* any assertion that can fatally exit the test
    # — otherwise a failing assert_true skips straight to the trap and this
    # diagnostic never gets seen, which is exactly what happened the first
    # time this test failed in CI (a blind 0-byte failure with no clue why).
    local exec_out
    exec_out="$(kubectl exec "$pod" -n "$TEST_NAMESPACE" -- sh -c 'cat /tmp/watch.err 2>/dev/null' 2>&1)"
    echo "=== watch.err ==="
    echo "$exec_out"

    # Two positive checks, not just "no error substring" (which would pass
    # vacuously if the exec itself silently failed, or watch.err doesn't
    # exist yet): the watch must have actually received real bytes, and
    # curl must still be running (not have exited at all — an exit for any
    # reason this soon, clean or otherwise, means the connection didn't
    # survive to keep streaming).
    local watch_bytes curl_pid_count
    watch_bytes="$(kubectl exec "$pod" -n "$TEST_NAMESPACE" -- sh -c 'wc -c < /tmp/watch.out 2>/dev/null' 2>&1)"
    curl_pid_count="$(kubectl exec "$pod" -n "$TEST_NAMESPACE" -- sh -c 'pgrep curl | wc -l' 2>&1)"
    echo "watch.out bytes=$watch_bytes curl_processes_still_running=$curl_pid_count"
    assert_true test "${watch_bytes:-0}" -gt 0
    assert_true test "${curl_pid_count:-0}" -gt 0

    assert_not_contains "$exec_out" "connection reset by peer" \
        "a burst of Service churn must not reset a long-lived pod-to-apiserver connection"

    for i in $(seq 1 25); do
        kubectl delete svc "churn-svc-$i" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true
    done
    trap - EXIT
    kubectl delete pod "$pod" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true
}

register_test test_a_long_lived_watch_survives_a_service_churn_burst
