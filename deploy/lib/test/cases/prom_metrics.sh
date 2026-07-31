# lib/test/cases/prom_metrics.sh — /metrics/resource and /metrics/cadvisor
# (server::prom_metrics), the Prometheus-text alternatives to
# /stats/summary. Like stats.sh, this only proves nodelet's endpoints work
# — a real Prometheus scrape config pointed at them is out of scope here.

test_metrics_resource_returns_real_pod_usage() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="metrics-resource-check"
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

    local node_ip token body
    node_ip="$(kubectl get node "$(node_name)" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')"
    token="$(kubectl create token default --duration=5m 2>/dev/null || kubectl -n default create token default 2>/dev/null)"
    if [[ -z "$token" ]]; then
        delete_pod_if_exists "$name"
        skip_test "couldn't mint a bearer token (kubectl create token) to call the server directly"
    fi

    if ! body="$(try_wait_until 30 bash -c "curl -ksS --max-time 5 -H 'Authorization: Bearer $token' https://$node_ip:${NODELET_SERVER_PORT:-10250}/metrics/resource | grep -q 'pod=\"$name\"'" \
        && curl -ksS --max-time 5 -H "Authorization: Bearer $token" "https://$node_ip:${NODELET_SERVER_PORT:-10250}/metrics/resource")"; then
        delete_pod_if_exists "$name"
        die "/metrics/resource never mentioned pod '$name' — check nodelet's server logs, and that this node's firewall allows the test script to reach NODELET_SERVER_PORT directly"
    fi

    assert_contains "$body" "# TYPE node_cpu_usage_seconds_total counter" "node_cpu_usage_seconds_total TYPE line present"
    assert_contains "$body" "# TYPE container_memory_working_set_bytes gauge" "container_memory_working_set_bytes TYPE line present"
    assert_contains "$body" "pod=\"$name\"" "the running pod appears in the resource metrics"
    delete_pod_if_exists "$name"
}

test_metrics_cadvisor_returns_real_container_usage() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="metrics-cadvisor-check"
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

    local node_ip token body
    node_ip="$(kubectl get node "$(node_name)" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')"
    token="$(kubectl create token default --duration=5m 2>/dev/null || kubectl -n default create token default 2>/dev/null)"
    if [[ -z "$token" ]]; then
        delete_pod_if_exists "$name"
        skip_test "couldn't mint a bearer token (kubectl create token) to call the server directly"
    fi

    if ! body="$(try_wait_until 30 bash -c "curl -ksS --max-time 5 -H 'Authorization: Bearer $token' https://$node_ip:${NODELET_SERVER_PORT:-10250}/metrics/cadvisor | grep -q 'pod=\"$name\"'" \
        && curl -ksS --max-time 5 -H "Authorization: Bearer $token" "https://$node_ip:${NODELET_SERVER_PORT:-10250}/metrics/cadvisor")"; then
        delete_pod_if_exists "$name"
        die "/metrics/cadvisor never mentioned pod '$name' — check nodelet's server logs"
    fi

    assert_contains "$body" "# TYPE container_cpu_usage_seconds_total counter" "container_cpu_usage_seconds_total TYPE line present"
    assert_contains "$body" "container=\"app\"" "the running container appears in the cadvisor metrics"
    delete_pod_if_exists "$name"
}

register_test test_metrics_resource_returns_real_pod_usage
register_test test_metrics_cadvisor_returns_real_container_usage
