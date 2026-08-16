test_diag5_in_cluster_watch() {
    local pod="diag5-watcher"
    apply_manifest <<PODEOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  serviceAccountName: default
  containers:
    - name: watcher
      image: bitnami/kubectl:latest
      command: ["sh", "-c", "TOKEN=\$(cat /var/run/secrets/kubernetes.io/serviceaccount/token); CACERT=/var/run/secrets/kubernetes.io/serviceaccount/ca.crt; echo BEFORE_CURL; curl -v -sS --connect-timeout 5 --max-time 20 --cacert \$CACERT -H \"Authorization: Bearer \$TOKEN\" \"https://kubernetes.default.svc/api/v1/namespaces/$TEST_NAMESPACE/persistentvolumeclaims\" > /tmp/plain.out 2>/tmp/plain.err; echo PLAIN_EXIT=\$?; echo BEFORE_WATCH_CURL; curl -v -sS -N --connect-timeout 5 --max-time 45 --cacert \$CACERT -H \"Authorization: Bearer \$TOKEN\" \"https://kubernetes.default.svc/api/v1/namespaces/$TEST_NAMESPACE/persistentvolumeclaims?watch=true&timeoutSeconds=40\" > /tmp/watch.out 2>/tmp/watch.err; echo WATCH_EXIT=\$?; sleep 3600"]
PODEOF
    trap 'kubectl delete pod "$pod" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    try_wait_until 30 pod_is_phase "$pod" Running || die "diag5 pod never started"
    sleep 2
    local claim="diag5-claim"
    apply_manifest <<CLAIMEOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $claim
spec:
  accessModes: ["ReadWriteOnce"]
  resources:
    requests:
      storage: 64Mi
CLAIMEOF

    sleep 50
    echo "=== DIAG5: pod logs (plain GET then watch) ==="
    kubectl logs "$pod" -n "$TEST_NAMESPACE" || true
    echo "=== DIAG5: plain.err ==="
    kubectl exec "$pod" -n "$TEST_NAMESPACE" -- cat /tmp/plain.err 2>&1 || true
    echo "=== DIAG5: watch.err (partial, curl -v trace) ==="
    kubectl exec "$pod" -n "$TEST_NAMESPACE" -- sh -c 'tail -c 4000 /tmp/watch.err' 2>&1 || true

    kubectl delete pvc "$claim" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true
    trap - EXIT
    kubectl delete pod "$pod" -n "$TEST_NAMESPACE" --ignore-not-found >/dev/null 2>&1 || true
    die "diag5 dump above"
}
register_test test_diag5_in_cluster_watch
